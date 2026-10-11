#!/bin/sh
# End-to-end power tests of the neolink binary against the scripted fake camera
# (SPEC A23, A24, A26, A28, A29, A30, reduced to minutes of real time).
#
# Runs inside the dev container (mosquitto, ffmpeg and curl on PATH) after
#   cargo build --release -p neolink -p xtask
#   cargo xtask mkfixture --standard --out fixtures/synth
# Usage: sh tests/e2e/power.sh   (exit status 0 = all passed)
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
W=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$W"
export NEOLINK_STATE_DIR=$W/state
FAILS=0
PIDS=""

cleanup() { for p in $PIDS; do kill -TERM "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT

stat() { sed -n "s/.*\"$1\":\([0-9]*\).*/\1/p" "$W/stats.json"; }
check() { # description, condition (shell test)
  if eval "$2"; then echo "PASS $1"; else echo "FAIL $1 ($2; stats: $(cat "$W/stats.json"))"; FAILS=$((FAILS+1)); fi
}
wait_for() { # file pattern seconds
  i=0; until grep -q "$2" "$1" 2>/dev/null; do i=$((i+1)); [ $i -gt $(($3*10)) ] && return 1; sleep 0.1; done
}

# --- fake camera and broker
"$BIN/xtask" fakecam --addr-file "$W/addr" --stats "$W/stats.json" \
  --main "$ROOT/fixtures/synth/plain_h264.bcm" --sub "$ROOT/fixtures/synth/plain_h264.bcm" 2> "$W/fakecam.log" &
PIDS="$PIDS $!"
wait_for "$W/addr" . 10 || { echo "fake camera did not start"; exit 1; }
ADDR=$(cat "$W/addr")
mosquitto -p 18883 > "$W/mosquitto.log" 2>&1 &
PIDS="$PIDS $!"
sleep 0.5

# Retained commands from "before we started": they must be ignored (A23)
mosquitto_pub -p 18883 -r -t neolink/fake/control/wakeup -m 5
mosquitto_pub -p 18883 -r -t neolink/fake/query/battery -m ""

cat > "$W/neolink.toml" <<EOF
bind = "127.0.0.1"
bind_port = 18554

[mqtt]
broker_addr = "127.0.0.1"
port = 18883

[[cameras]]
name = "fake"
username = "admin"
password = "fakepass"
address = "$ADDR"
idle_grace_s = 2
EOF

RUST_LOG=info "$BIN/neolink" mqtt-rtsp --config "$W/neolink.toml" > "$W/neolink.log" 2>&1 &
NEOLINK=$!
PIDS="$PIDS $NEOLINK"

# --- A30: mounts exist before any wake (P16); A23: boot and idle make no contact
wait_for "$W/neolink.log" "fake: Available at.*main" 30
check "A30 mounts before any wake" 'grep -q "fake: Available at.*main" "$W/neolink.log"'
# A DESCRIBE-less health probe and an unchanged config re-publish while idle
sleep 3
mosquitto_pub -p 18883 -t neolink/config -m "$(cat "$W/neolink.toml")" 2>/dev/null
for i in 1 2 3 4 5; do curl -s -o /dev/null "http://127.0.0.1:18555/fake/snapshot.jpg"; sleep 1; done
check "A23 idle boot: 0 connections" '[ "$(stat connections)" = 0 ]'
check "A23 retained commands ignored" 'grep -q "Ignoring retained MQTT message on neolink/fake/control/wakeup" "$W/neolink.log"'

# --- A24: on-demand cycles; each wakes once and is released with a logout
for cycle in 1 2 3; do
  timeout 30 ffmpeg -v error -nostdin -rtsp_transport tcp -i rtsp://127.0.0.1:18554/fake/main -t 3 -f null - > "$W/ffmpeg$cycle.log" 2>&1
  RC=$?
  check "A24 cycle $cycle: ffmpeg played" '[ $RC = 0 ]'
  i=0; while [ "$(stat open_connections)" != 0 ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done
  sleep 1
  check "A24 cycle $cycle: released" '[ "$(stat open_connections)" = 0 ]'
done
check "A24 3 logins" '[ "$(stat logins)" = 3 ]'
check "A24 3 logouts" '[ "$(stat logouts)" = 3 ]'
check "A24 ledger lines" '[ $(grep -c "fake: awake .* for stream:" "$W/neolink.log") -ge 3 ]'

# --- A26: snapshots are served from the cache without waking
sleep 3
BEFORE=$(stat connections)
for i in $(seq 1 20); do curl -s -o "$W/snap.jpg" -D "$W/snap.h" "http://127.0.0.1:18555/fake/snapshot.jpg"; done
check "A26 20 snapshots: 0 new connections" '[ "$(stat connections)" = "$BEFORE" ]'
check "A26 snapshot is the camera JPEG" 'grep -q "fake jpeg" "$W/snap.jpg"'
check "A26 Last-Modified" 'grep -qi "^Last-Modified: " "$W/snap.h"'

# --- A29: the CLI reads the state file and makes no connection
BEFORE=$(stat connections)
"$BIN/neolink" battery --config "$W/neolink.toml" fake > "$W/battery.out" 2> "$W/battery.err"
check "A29 CLI battery from state" 'grep -q "<batteryPercent>80</batteryPercent>" "$W/battery.out"'
check "A29 CLI battery: 0 connections" '[ "$(stat connections)" = "$BEFORE" ]'
check "A29 second process warned" 'grep -q "Another neolink process" "$W/battery.err"'

# --- A28: unreachable camera, a query arrives: it fails at its deadline, no retry after
kill -USR2 "$(pgrep -f 'xtask fakecam')"
mosquitto_pub -p 18883 -t neolink/fake/query/battery -m ""
sleep 2
check "A28 query caused attempts" '[ "$(stat connections)" -gt "$BEFORE" ]'
sleep 65
ATTEMPTS=$(stat connections)
sleep 10
check "A28 no retries after the deadline" '[ "$(stat connections)" = "$ATTEMPTS" ]'

# --- shutdown while streaming: logs out, exits within 5 s (PLAN 1.5, N9)
kill -USR2 "$(pgrep -f 'xtask fakecam')"   # reachable again
sleep 1
LOGINS=$(stat logins); LOGOUTS=$(stat logouts)
timeout 60 ffmpeg -v error -nostdin -rtsp_transport tcp -i rtsp://127.0.0.1:18554/fake/main -t 30 -f null - > /dev/null 2>&1 &
i=0; while [ "$(stat logins)" = "$LOGINS" ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done
sleep 2
kill -TERM "$NEOLINK"
i=0; while kill -0 "$NEOLINK" 2>/dev/null && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
check "N9 neolink exits within 5 s of SIGTERM" '! kill -0 "$NEOLINK" 2>/dev/null'
sleep 0.5
check "1.5 SIGTERM logs the camera out" '[ "$(stat logouts)" -gt "$LOGOUTS" ]'
check "1.14 no Thread panicked from the CLI" '! grep -q "Thread panicked" "$W/battery.err"'

echo "logs in $W"
[ $FAILS = 0 ] && echo "ALL PASSED" || echo "$FAILS FAILED"
exit $FAILS
