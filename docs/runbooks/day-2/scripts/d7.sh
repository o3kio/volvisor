#!/usr/bin/env bash
# D7: scoped credential rotation and same-host TLS certificate rotation.
#
# A: add-new -> restart -> overlap -> remove-old -> restart. The old token
#    keeps working only during the overlap window and is rejected after
#    the removal restart; there is no hidden fallback.
# B: replace the configured PEM files without restarting: the served
#    certificate fingerprint is unchanged (no hot reload). Restart and the
#    new certificate is served.
source "$(dirname "$0")/lib.sh"

TAG="d7"
begin_drill "07-credential-and-cert-rotation"
trap 'stop_pidfile "$TAG-daemon" TERM >/dev/null 2>&1 || true' EXIT

# ---------------------------------------------------------------- part A
say "A: scoped API credential rotation (no hot reload)"
JOURNAL="$RUN/lab-$TAG/journal"
mkdir -p "$JOURNAL"
PORT="$(pick_port)"
BASE="http://127.0.0.1:$PORT"
OLD="d7-disposable-admin-old-$TAG"
NEW="d7-disposable-admin-new-$TAG"
OP="d7-disposable-operator-$TAG"
OLD_ID="admin-2026-11-a"
NEW_ID="admin-2026-11-b"
OP_ID="operator-2026-11-a"
CFG1="$RUN/configs/$TAG-cred-1.toml"
CFG2="$RUN/configs/$TAG-cred-2.toml"
CFG3="$RUN/configs/$TAG-cred-3.toml"

write_header_config "$RUN/configs/$TAG-old.curl" "$OLD"
write_header_config "$RUN/configs/$TAG-new.curl" "$NEW"
write_header_config "$RUN/configs/$TAG-op.curl" "$OP"
OLD_CURL="$RUN/configs/$TAG-old.curl"
NEW_CURL="$RUN/configs/$TAG-new.curl"
OP_CURL="$RUN/configs/$TAG-op.curl"

cat >"$CFG1" <<EOF
listen = "127.0.0.1:$PORT"
journal_dir = "$JOURNAL"
provider = "fake"

[[credentials.admin]]
id = "$OLD_ID"
token = "$OLD"

[[credentials.operator]]
id = "$OP_ID"
token = "$OP"
EOF
cat >"$CFG2" <<EOF
listen = "127.0.0.1:$PORT"
journal_dir = "$JOURNAL"
provider = "fake"

[[credentials.admin]]
id = "$OLD_ID"
token = "$OLD"

[[credentials.admin]]
id = "$NEW_ID"
token = "$NEW"

[[credentials.operator]]
id = "$OP_ID"
token = "$OP"
EOF
cat >"$CFG3" <<EOF
listen = "127.0.0.1:$PORT"
journal_dir = "$JOURNAL"
provider = "fake"

[[credentials.admin]]
id = "$NEW_ID"
token = "$NEW"

[[credentials.operator]]
id = "$OP_ID"
token = "$OP"
EOF

for i in a b c d; do
  cat >"$RUN/records/$TAG-cred-$i.json" <<EOF
{"api_version":"volvisor.volume.v2","operation_id":"op-d7-cred-$i","project_id":"day2","volume_id":"vol-d7-cred-$i","class":"native-local","size_bytes":67108864}
EOF
done

cap "phase 1: validate the single-admin configuration" \
  "$BIN/volvisord" --check-config --config "$CFG1"
start_volvisord "$TAG-daemon" "$CFG1" "$PORT" || exit 1
cap "phase 1: the current admin token mutates" \
  api_expect "$OLD_CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-cred-a.json" 200 "$RUN/records/$TAG-cred-a.resp.json"
cap "phase 1: the current operator token mutates (role check)" \
  api_expect "$OP_CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-cred-b.json" 200 "$RUN/records/$TAG-cred-b.resp.json"
cap "phase 1: the not-yet-issued new admin token is rejected" \
  api_expect "$NEW_CURL" GET "$BASE/v2/volumes" "" 401 "$RUN/records/$TAG-new-early.json"
stop_pidfile "$TAG-daemon" TERM

say "phase 2: add the new admin entry, restart, and prove the overlap"
cap "phase 2: validate the two-admin overlap configuration" \
  "$BIN/volvisord" --check-config --config "$CFG2"
start_volvisord "$TAG-daemon" "$CFG2" "$PORT" || exit 1
cap "phase 2: the old admin token still mutates during overlap" \
  api_expect "$OLD_CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-cred-c.json" 200 "$RUN/records/$TAG-cred-c.resp.json"
cap "phase 2: the new admin token mutates during overlap" \
  api_expect "$NEW_CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-cred-d.json" 200 "$RUN/records/$TAG-cred-d.resp.json"
cap "phase 2: both tokens can read the volume list" \
  api_expect "$OLD_CURL" GET "$BASE/v2/volumes" "" 200 "$RUN/records/$TAG-list-overlap.json"
stop_pidfile "$TAG-daemon" TERM

say "phase 3: remove the old admin entry, restart, and prove fail-closed removal"
cap "phase 3: validate the rotated configuration" \
  "$BIN/volvisord" --check-config --config "$CFG3"
start_volvisord "$TAG-daemon" "$CFG3" "$PORT" || exit 1
cap "phase 3: the removed old admin token is rejected" \
  api_expect "$OLD_CURL" GET "$BASE/v2/volumes" "" 401 "$RUN/records/$TAG-old-removed.json"
cap "phase 3: the removed old token cannot mutate" \
  api_expect "$OLD_CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-cred-a.json" 401 "$RUN/records/$TAG-old-removed-mutate.json"
cap "phase 3: the new admin token still mutates" \
  api_expect "$NEW_CURL" POST "$BASE/v2/volumes" "$RUN/records/$TAG-cred-b.json" 200 "$RUN/records/$TAG-cred-new-final.json"
stop_pidfile "$TAG-daemon" TERM

# ---------------------------------------------------------------- part B
say "B: same-host TLS certificate rotation (no hot reload)"
CERTDIR="$RUN/certs/$TAG"
mkdir -p "$CERTDIR"
CA="$CERTDIR/ca.crt"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$CERTDIR/ca.key" -out "$CA" \
  -subj "/CN=Day2 Drill CA" -days 2 >"$RUN/raw/$TAG-ca.log" 2>&1
openssl req -newkey rsa:2048 -nodes -keyout "$CERTDIR/server1.key" -out "$CERTDIR/server1.csr" \
  -subj "/CN=localhost" >"$RUN/raw/$TAG-csr1.log" 2>&1
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\n' >"$CERTDIR/san.ext"
openssl x509 -req -in "$CERTDIR/server1.csr" -CA "$CA" -CAkey "$CERTDIR/ca.key" \
  -CAcreateserial -out "$CERTDIR/server1.crt" -days 2 -extfile "$CERTDIR/san.ext" \
  >"$RUN/raw/$TAG-sign1.log" 2>&1
openssl req -newkey rsa:2048 -nodes -keyout "$CERTDIR/server2.key" -out "$CERTDIR/server2.csr" \
  -subj "/CN=localhost" >"$RUN/raw/$TAG-csr2.log" 2>&1
openssl x509 -req -in "$CERTDIR/server2.csr" -CA "$CA" -CAkey "$CERTDIR/ca.key" \
  -CAcreateserial -out "$CERTDIR/server2.crt" -days 2 -extfile "$CERTDIR/san.ext" \
  >"$RUN/raw/$TAG-sign2.log" 2>&1
# The daemon is configured with the *stable* paths server.crt/server.key;
# rotation replaces the bytes at those paths.
cp "$CERTDIR/server1.crt" "$CERTDIR/server.crt"
cp "$CERTDIR/server1.key" "$CERTDIR/server.key"
chmod 600 "$CERTDIR/server.key"
FP1="$(openssl x509 -in "$CERTDIR/server1.crt" -noout -fingerprint -sha256 | sed 's/^.*=//')"
FP2="$(openssl x509 -in "$CERTDIR/server2.crt" -noout -fingerprint -sha256 | sed 's/^.*=//')"
printf 'cert1 fingerprint=%s\ncert2 fingerprint=%s\n' "$FP1" "$FP2" | tee -a "$LOG"
assert_eq "$([[ "$FP1" != "$FP2" ]] && echo distinct || echo same)" "distinct" "the two certificates differ"

TJOURNAL="$RUN/lab-$TAG/tls-journal"
TPORT="$(pick_port)"
TCFG="$RUN/configs/$TAG-tls.toml"
TTOKEN="d7-disposable-tls-admin-$TAG"
write_header_config "$RUN/configs/$TAG-tls.curl" "$TTOKEN"
printf 'cacert = "%s"\n' "$CA" >>"$RUN/configs/$TAG-tls.curl"
TCURL="$RUN/configs/$TAG-tls.curl"
TBASE="https://127.0.0.1:$TPORT"
cat >"$TCFG" <<EOF
listen = "127.0.0.1:$TPORT"
journal_dir = "$TJOURNAL"
provider = "fake"
admin_token = "$TTOKEN"

[tls]
enabled = true
cert_file = "$CERTDIR/server.crt"
key_file = "$CERTDIR/server.key"
EOF

served_fp() {
  openssl s_client -connect "127.0.0.1:$TPORT" -servername localhost -CAfile "$CA" </dev/null 2>/dev/null \
    | openssl x509 -noout -fingerprint -sha256 2>/dev/null | sed 's/^.*=//'
}

start_volvisord_tls() {
  local name="$1" cfg="$2" port="$3"
  local log="$RUN/raw/$name.daemon.log"
  "$BIN/volvisord" --config "$cfg" >"$log" 2>&1 &
  echo "$!" >"$RUN/records/$name.pid"
  local i
  for ((i = 0; i < 300; i++)); do
    if curl -fsS --max-time 1 --cacert "$CA" "https://127.0.0.1:$port/healthz" >/dev/null 2>&1; then
      printf 'daemon %s healthy pid=%s port=%s tls=true\n' "$name" "$(cat "$RUN/records/$name.pid")" "$port" | tee -a "$LOG"
      return 0
    fi
    sleep 0.1
  done
  printf 'TLS DAEMON FAILED TO BECOME HEALTHY: %s (pid %s)\n' "$name" "$(cat "$RUN/records/$name.pid")" | tee -a "$LOG"
  tail -40 "$log" | tee -a "$LOG"
  return 1
}

start_volvisord_tls "$TAG-daemon" "$TCFG" "$TPORT" || exit 1
cap "HTTPS health check with the private CA" curl -fsS --cacert "$CA" "$TBASE/healthz"
SERVED_BEFORE="$(served_fp)"
printf 'served fingerprint before rotation=%s\n' "$SERVED_BEFORE" | tee -a "$LOG"
assert_eq "$SERVED_BEFORE" "$FP1" "initial served certificate"

say "replace the PEM bytes at the configured paths without restarting"
cp "$CERTDIR/server2.crt" "$CERTDIR/server.crt"
cp "$CERTDIR/server2.key" "$CERTDIR/server.key"
chmod 600 "$CERTDIR/server.key"
cap "on-disk certificate after the swap" sha256sum "$CERTDIR/server.crt" "$CERTDIR/server.key"
SERVED_HOT="$(served_fp)"
printf 'served fingerprint after file swap (no restart)=%s\n' "$SERVED_HOT" | tee -a "$LOG"
assert_eq "$SERVED_HOT" "$FP1" "no hot reload: still serving the old certificate"
cap "HTTPS still works with the old in-memory certificate" curl -fsS --cacert "$CA" "$TBASE/healthz"

stop_pidfile "$TAG-daemon" TERM
start_volvisord_tls "$TAG-daemon" "$TCFG" "$TPORT" || exit 1
SERVED_RESTART="$(served_fp)"
printf 'served fingerprint after restart=%s\n' "$SERVED_RESTART" | tee -a "$LOG"
assert_eq "$SERVED_RESTART" "$FP2" "restart serves the replacement certificate"
cap "HTTPS works with the rotated certificate" curl -fsS --cacert "$CA" "$TBASE/healthz"
cap "new TLS session details (protocol and subject)" \
  openssl s_client -connect "127.0.0.1:$TPORT" -servername localhost -CAfile "$CA" </dev/null 2>/dev/null
cap "an admin request over the rotated TLS listener" \
  api_expect "$TCURL" POST "$TBASE/v2/volumes" "$RUN/records/$TAG-cred-a.json" 200 "$RUN/records/$TAG-tls-create.json"
stop_pidfile "$TAG-daemon" TERM

if end_drill; then
  printf 'D7 COMPLETE\n'
else
  printf 'D7 FAILED\n'
  exit 1
fi
