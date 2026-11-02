#!/usr/bin/env bash
set -euo pipefail

REPO=${REPO:-$(git -C "$(dirname "$0")" rev-parse --show-toplevel)}
BIN="$REPO/target/debug/volvisord"
ROOT=${ROOT:-/tmp/opencode/transport-tls-evidence}
NS_A=vv-tls-a
NS_B=vv-tls-b
TOKEN='EVIDENCE-READER-TOKEN-DO-NOT-USE'

cleanup() {
  set +e
  if [[ -n "${SERVER_PID:-}" ]]; then kill "$SERVER_PID" 2>/dev/null; wait "$SERVER_PID" 2>/dev/null; fi
  ip netns del "$NS_A" 2>/dev/null
  ip netns del "$NS_B" 2>/dev/null
}
trap cleanup EXIT

rm -rf "$ROOT"
mkdir -p "$ROOT/journal"
chmod 700 "$ROOT"

# Private CA and a server identity for the veth peer address.
openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 2 \
  -keyout "$ROOT/ca.key" -out "$ROOT/ca.crt" \
  -subj '/CN=Volvisor transport evidence CA' >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -sha256 \
  -keyout "$ROOT/server.key" -out "$ROOT/server.csr" \
  -subj '/CN=volvisor-transport-evidence' >/dev/null 2>&1
cat >"$ROOT/server.ext" <<'EOF'
subjectAltName=IP:10.200.40.2
extendedKeyUsage=serverAuth
EOF
openssl x509 -req -in "$ROOT/server.csr" -CA "$ROOT/ca.crt" -CAkey "$ROOT/ca.key" \
  -CAcreateserial -out "$ROOT/server.crt" -days 2 -sha256 \
  -extfile "$ROOT/server.ext" >/dev/null 2>&1

cat >"$ROOT/volvisord.toml" <<EOF
listen = "10.200.40.2:8443"
journal_dir = "$ROOT/journal"
provider = "fake"
admin_token = "EVIDENCE-LEGACY-ADMIN-TOKEN-DO-NOT-USE"

[[credentials.admin]]
id = "admin-evidence"
token = "EVIDENCE-ADMIN-TOKEN-DO-NOT-USE"

[[credentials.operator]]
id = "operator-evidence"
token = "EVIDENCE-OPERATOR-TOKEN-DO-NOT-USE"

[[credentials.read_only]]
id = "reader-evidence"
token = "$TOKEN"

[tls]
enabled = true
cert_file = "$ROOT/server.crt"
key_file = "$ROOT/server.key"
EOF

cat >"$ROOT/insecure.toml" <<EOF
listen = "10.200.40.2:8443"
journal_dir = "$ROOT/journal"
provider = "fake"
admin_token = "EVIDENCE-ADMIN-TOKEN-DO-NOT-USE"
EOF

# Refusal of a non-loopback plain-HTTP listener without allow_insecure.
set +e
"$BIN" --config "$ROOT/insecure.toml" --check-config >"$ROOT/insecure-refusal.txt" 2>&1
REFUSAL_STATUS=$?
set -e
printf 'exit status: %s\n' "$REFUSAL_STATUS" >>"$ROOT/insecure-refusal.txt"

ip netns add "$NS_A"
ip netns add "$NS_B"
ip link add veth-a type veth peer name veth-b
ip link set veth-a netns "$NS_A"
ip link set veth-b netns "$NS_B"
ip netns exec "$NS_A" ip addr add 10.200.40.2/24 dev veth-a
ip netns exec "$NS_A" ip link set veth-a up
ip netns exec "$NS_A" ip link set lo up
ip netns exec "$NS_B" ip addr add 10.200.40.1/24 dev veth-b
ip netns exec "$NS_B" ip link set veth-b up
ip netns exec "$NS_B" ip link set lo up

ip netns exec "$NS_A" "$BIN" --config "$ROOT/volvisord.toml" >"$ROOT/server.log" 2>&1 &
SERVER_PID=$!

# Wait for the TLS listener to answer the public liveness probe.
for _ in $(seq 1 100); do
  if ip netns exec "$NS_B" curl -sS --cacert "$ROOT/ca.crt" \
      -o "$ROOT/healthz.json" -w '%{http_code}' \
      "https://10.200.40.2:8443/healthz" >"$ROOT/healthz.status" 2>"$ROOT/healthz.err"; then
    break
  fi
  sleep 0.1
done

# An untrusted client must not talk to the listener: same URL, default roots.
set +e
ip netns exec "$NS_B" curl -sS -o /dev/null \
  "https://10.200.40.2:8443/healthz" >"$ROOT/curl-untrusted.stdout" 2>"$ROOT/curl-untrusted.stderr"
UNTRUSTED_STATUS=$?
set -e
printf 'exit status: %s\n' "$UNTRUSTED_STATUS" >"$ROOT/curl-untrusted-result.txt"
cat "$ROOT/curl-untrusted.stderr" >>"$ROOT/curl-untrusted-result.txt"

# Capture the encrypted GET carrying a bearer token.
ip netns exec "$NS_B" tcpdump -i veth-b -s 0 -U -w "$ROOT/tls.pcap" 'tcp port 8443' \
  >"$ROOT/tcpdump-capture.log" 2>&1 &
TCPDUMP_PID=$!
sleep 1
ip netns exec "$NS_B" curl -sS --cacert "$ROOT/ca.crt" \
  -D "$ROOT/curl-with-ca.headers" -o "$ROOT/curl-with-ca.body" \
  -H "Authorization: Bearer $TOKEN" \
  "https://10.200.40.2:8443/v2/volumes" >"$ROOT/curl-with-ca.stdout" 2>"$ROOT/curl-with-ca.stderr"
sleep 1
kill -INT "$TCPDUMP_PID"
wait "$TCPDUMP_PID" 2>/dev/null || true

tcpdump -nn -r "$ROOT/tls.pcap" >"$ROOT/tcpdump-summary.txt" 2>&1
tcpdump -A -r "$ROOT/tls.pcap" >"$ROOT/tcpdump-ascii.txt" 2>&1

{
  printf 'token scan: '
  if grep -a -F "$TOKEN" "$ROOT/tcpdump-ascii.txt" >/dev/null; then
    printf 'FOUND (plaintext leak)\n'
  else
    printf 'NOT FOUND (encrypted on the wire)\n'
  fi
  printf 'volume request path scan: '
  if grep -a -F 'GET /v2/volumes' "$ROOT/tcpdump-ascii.txt" >/dev/null; then
    printf 'FOUND (plaintext leak)\n'
  else
    printf 'NOT FOUND (encrypted on the wire)\n'
  fi
  printf 'bearer header scan: '
  if grep -a -F 'Authorization: Bearer' "$ROOT/tcpdump-ascii.txt" >/dev/null; then
    printf 'FOUND (plaintext leak)\n'
  else
    printf 'NOT FOUND (encrypted on the wire)\n'
  fi
} >"$ROOT/plaintext-scan.txt"

# The token must also be absent from the daemon's structured log.
{
  printf 'server-log reader token scan: '
  if grep -F "$TOKEN" "$ROOT/server.log" >/dev/null; then
    printf 'FOUND (log leak)\n'
  else
    printf 'NOT FOUND\n'
  fi
  printf 'server-log volume-id scan: '
  if grep -F 'volumes' "$ROOT/server.log" >/dev/null; then
    printf 'FOUND (expected route name only)\n'
  else
    printf 'NOT FOUND\n'
  fi
} >"$ROOT/log-scan.txt"

openssl x509 -in "$ROOT/server.crt" -noout -subject -issuer -ext subjectAltName \
  >"$ROOT/certificate.txt" 2>&1
sha256sum "$ROOT/tls.pcap" "$ROOT/server.crt" "$ROOT/ca.crt" >"$ROOT/sha256.txt"
{
  printf 'repo commit: '; git -C "$REPO" rev-parse HEAD
  printf 'binary: '; "$BIN" --version
  printf 'kernel: '; uname -a
  printf 'curl: '; curl --version | head -1
  printf 'openssl: '; openssl version
} >"$ROOT/versions.txt"

printf 'capture complete: %s\n' "$ROOT"
