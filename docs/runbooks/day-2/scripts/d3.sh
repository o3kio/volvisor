#!/usr/bin/env bash
# D3: lost / unreachable writer-authority witness (service level).
#
# Starts a local volvisor-witnessd, registers a volume with two endpoint
# hosts, grants host-a an epoch/lease, stops the witness, proves the
# authority surface is unreachable (connection refused), restarts it and
# proves the registration/epoch/lease were recovered from the witness
# journal. Then self-releases host-a, waits out the fence window, grants
# host-b a strictly newer epoch and records the fencing proof.
#
# LIMIT: this drills the witness service, not the storage-side lease
# enforcement. No DRBD 9 module / peered resource exists on this host, so
# a writer that keeps writing after losing the witness is not exercised.
# That half stays PARTIAL in the report.
source "$(dirname "$0")/lib.sh"

TAG="d3"
begin_drill "03-lost-witness"
trap 'stop_pidfile "$TAG-witness" TERM >/dev/null 2>&1 || true' EXIT

DIR="$RUN/lab-$TAG"
STATE_DIR="$DIR/witness-state"
CFG="$DIR/witnessd.toml"
mkdir -p "$STATE_DIR"
PORT="$(pick_port)"
ADMIN="d3-disposable-admin-$TAG"
TOKEN_A="d3-disposable-host-a-$TAG"
TOKEN_B="d3-disposable-host-b-$TAG"
BASE="http://127.0.0.1:$PORT"

cat >"$CFG" <<EOF
listen = "127.0.0.1:$PORT"
state_dir = "$STATE_DIR"
auth_token = "$ADMIN"
lease_ttl_secs = 120
lease_grace_secs = 2
suspend_budget_secs = 2

[host_tokens]
host-a = "$TOKEN_A"
host-b = "$TOKEN_B"
EOF

write_header_config "$RUN/configs/$TAG-admin.curl" "$ADMIN"
write_header_config "$RUN/configs/$TAG-host-a.curl" "$TOKEN_A"
write_header_config "$RUN/configs/$TAG-host-b.curl" "$TOKEN_B"
ADMIN_CURL="$RUN/configs/$TAG-admin.curl"
A_CURL="$RUN/configs/$TAG-host-a.curl"
B_CURL="$RUN/configs/$TAG-host-b.curl"

start_witnessd() {
  local log="$RUN/raw/$TAG-witness.daemon.log"
  "$BIN/volvisor-witnessd" --config "$CFG" >"$log" 2>&1 &
  echo "$!" >"$RUN/records/$TAG-witness.pid"
  local i
  for ((i = 0; i < 300; i++)); do
    if curl -fsS --max-time 1 "$BASE/healthz" >/dev/null 2>&1; then
      printf 'witness healthy pid=%s port=%s\n' "$(cat "$RUN/records/$TAG-witness.pid")" "$PORT" | tee -a "$LOG"
      return 0
    fi
    sleep 0.1
  done
  printf 'WITNESS FAILED TO BECOME HEALTHY\n' | tee -a "$LOG"
  tail -40 "$log" | tee -a "$LOG"
  return 1
}

cap "validate the witness configuration before starting" \
  "$BIN/volvisor-witnessd" --check-config --config "$CFG"
start_witnessd || exit 1

VOL="vol-d3-witness"
cat >"$RUN/records/$TAG-register.json" <<'EOF'
{
  "protocol_version": 2,
  "operation_id": "op-d3-register",
  "content": {
    "lineage_uuids": ["0000000000000004", "0000000000000005"],
    "endpoints": [
      {"host_id": "host-a", "backing": "vg-near/vol-d3-00000004", "volvisor_created": true},
      {"host_id": "host-b", "backing": "vg-near/vol-d3-00000004", "volvisor_created": false}
    ]
  }
}
EOF
cat >"$RUN/records/$TAG-grant-a.json" <<'EOF'
{"protocol_version": 2, "operation_id": "op-d3-grant-a", "host_id": "host-a"}
EOF
cat >"$RUN/records/$TAG-grant-b-early.json" <<'EOF'
{"protocol_version": 2, "operation_id": "op-d3-grant-b-early", "host_id": "host-b"}
EOF

cap "legacy shared token is read-only on the v2 witness (health)"
api_expect "$ADMIN_CURL" GET "$BASE/healthz" "" 200 "$RUN/records/$TAG-health.txt"
cap "host-a registers the volume (two hosts, two endpoints)" \
  api_expect "$A_CURL" POST "$BASE/v1/volumes/$VOL/register" "$RUN/records/$TAG-register.json" 200 "$RUN/records/$TAG-register.resp.json"
cat "$RUN/records/$TAG-register.resp.json" | tee -a "$LOG"
cap "host-a is granted writer epoch 1" \
  api_expect "$A_CURL" POST "$BASE/v1/volumes/$VOL/grant" "$RUN/records/$TAG-grant-a.json" 200 "$RUN/records/$TAG-grant-a.resp.json"
cat "$RUN/records/$TAG-grant-a.resp.json" | tee -a "$LOG"
cap "record the granted epoch/lease for the restart comparison" \
  jq '{epoch,lease_id,lease_ttl_secs}' "$RUN/records/$TAG-grant-a.resp.json"
GRANT_EPOCH="$(jq -r '.epoch' "$RUN/records/$TAG-grant-a.resp.json")"
GRANT_LEASE="$(jq -r '.lease_id' "$RUN/records/$TAG-grant-a.resp.json")"
assert_eq "$GRANT_EPOCH" "1" "initial granted epoch"

cap "inspect authority before the outage" \
  api_expect "$ADMIN_CURL" GET "$BASE/v1/volumes/$VOL" "" 200 "$RUN/records/$TAG-view-before.json"
jq '{volume_id,current_epoch,holder,lease_state,lease_id,commit_index}' "$RUN/records/$TAG-view-before.json" | tee -a "$LOG"
COMMIT_BEFORE="$(jq -r '.commit_index' "$RUN/records/$TAG-view-before.json")"

say "take the witness down (SIGTERM) and prove the authority surface is unreachable"
stop_pidfile "$TAG-witness" TERM
curl -sS --max-time 3 "$BASE/healthz" >"$RUN/records/$TAG-down-health.out" 2>"$RUN/records/$TAG-down-health.err"
DOWN_RC=$?
printf 'healthz during outage exit=%s stderr=%s\n' "$DOWN_RC" "$(tr '\n' ' ' <"$RUN/records/$TAG-down-health.err")" | tee -a "$LOG"
cap_stdin "grant while the witness is unavailable (transport failure, no authority minted)" \
  "$RUN/records/$TAG-grant-b-early.json" \
  curl -sS --max-time 3 --config "$B_CURL" --request POST \
  --header 'content-type: application/json' --data-binary @- \
  "$BASE/v1/volumes/$VOL/grant"
set +e
curl -sS --max-time 3 --config "$B_CURL" --request POST \
  --header 'content-type: application/json' \
  --data-binary @"$RUN/records/$TAG-grant-b-early.json" \
  "$BASE/v1/volumes/$VOL/grant" >/dev/null 2>"$RUN/records/$TAG-down-grant.err"
DOWN_GRANT_RC=$?
printf 'grant during outage exit=%s stderr=%s\n' "$DOWN_GRANT_RC" "$(tr '\n' ' ' <"$RUN/records/$TAG-down-grant.err")" | tee -a "$LOG"
assert_eq "$DOWN_RC" "7" "healthz connection-refused exit during outage"
assert_eq "$DOWN_GRANT_RC" "7" "grant connection-refused exit during outage"

say "restart the witness and prove the journal recovered registration, epoch and lease"
start_witnessd || exit 1
cap "inspect authority after restart" \
  api_expect "$ADMIN_CURL" GET "$BASE/v1/volumes/$VOL" "" 200 "$RUN/records/$TAG-view-after.json"
jq '{volume_id,current_epoch,holder,lease_state,lease_id,commit_index,registration}' \
  "$RUN/records/$TAG-view-after.json" | tee -a "$LOG"
assert_eq "$(jq -r '.current_epoch' "$RUN/records/$TAG-view-after.json")" "$GRANT_EPOCH" "epoch survived witness restart"
assert_eq "$(jq -r '.holder' "$RUN/records/$TAG-view-after.json")" "host-a" "holder survived witness restart"
assert_eq "$(jq -r '.lease_id' "$RUN/records/$TAG-view-after.json")" "$GRANT_LEASE" "lease survived witness restart"
assert_eq "$(jq -r '.commit_index' "$RUN/records/$TAG-view-after.json")" "$COMMIT_BEFORE" "commit index survived witness restart"
assert_eq "$(jq -r '.registration.endpoints | length' "$RUN/records/$TAG-view-after.json")" "2" "registration endpoints survived witness restart"

cap "a competing grant to host-b while host-a holds a live lease is refused" \
  api_expect "$B_CURL" POST "$BASE/v1/volumes/$VOL/grant" "$RUN/records/$TAG-grant-b-early.json" 409 "$RUN/records/$TAG-grant-b-early.resp.json"
jq '{code,message}' "$RUN/records/$TAG-grant-b-early.resp.json" | tee -a "$LOG"
assert_contains "$(jq -r '.code' "$RUN/records/$TAG-grant-b-early.resp.json")" "LEASE_HELD" "live-lease grant refusal code"

cat >"$RUN/records/$TAG-revoke-a.json" <<EOF
{"protocol_version": 2, "operation_id": "op-d3-revoke-a", "host_id": "host-a", "epoch": $GRANT_EPOCH}
EOF
cat >"$RUN/records/$TAG-grant-b.json" <<'EOF'
{"protocol_version": 2, "operation_id": "op-d3-grant-b", "host_id": "host-b"}
EOF
cap "host-a self-releases its lease" \
  api_expect "$A_CURL" POST "$BASE/v1/volumes/$VOL/revoke" "$RUN/records/$TAG-revoke-a.json" 200 "$RUN/records/$TAG-revoke-a.resp.json"
jq '{fencing_proof}' "$RUN/records/$TAG-revoke-a.resp.json" | tee -a "$LOG"

say "wait out the witness fence window (grace 2s + suspend budget 2s)"
sleep 5
cap "host-b is granted the next epoch only after the fence window" \
  api_expect "$B_CURL" POST "$BASE/v1/volumes/$VOL/grant" "$RUN/records/$TAG-grant-b.json" 200 "$RUN/records/$TAG-grant-b.resp.json"
jq '{epoch,lease_id,fencing_proof}' "$RUN/records/$TAG-grant-b.resp.json" | tee -a "$LOG"
assert_eq "$(jq -r '.epoch' "$RUN/records/$TAG-grant-b.resp.json")" "2" "successor epoch after failover"
cap "inspect the final authority state" \
  api_expect "$ADMIN_CURL" GET "$BASE/v1/volumes/$VOL" "" 200 "$RUN/records/$TAG-view-final.json"
jq '{current_epoch,holder,lease_state,lease_id,commit_index,retirements}' \
  "$RUN/records/$TAG-view-final.json" | tee -a "$LOG"

cap "the witness journal and lock are on disk" ls -l "$STATE_DIR"
stop_pidfile "$TAG-witness" TERM

if end_drill; then
  printf 'D3 COMPLETE\n'
else
  printf 'D3 FAILED\n'
  exit 1
fi
