#!/usr/bin/env bash
# End-to-end power tests of the neolink binary against the scripted fake camera
# (SPEC A23, A24, A26, A28, A29, A30, reduced to minutes of real time).
#
# Runs inside the dev container (mosquitto, ffmpeg and curl on PATH) after
#   cargo build --release -p neolink -p xtask
#   cargo xtask mkfixture --standard --out fixtures/synth
# Usage: bash tests/e2e/power.sh   (exit status: the number of failed checks; 1 when the
# run itself broke)
# The checks are strings that check() evals, so shellcheck sees their variables as unused
# shellcheck disable=SC2034
set -euo pipefail
. "$(dirname "$0")/lib.sh"
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
W=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$W"
export NEOLINK_STATE_DIR=$W/state
FAILS=0

stat() { need "$W/stats.json" "fake camera stats"; sed -n "s/.*\"$1\":\([0-9]*\).*/\1/p" "$W/stats.json"; }
check() { # description, condition (shell test)
  if eval "$2"; then echo "PASS $1"; else echo "FAIL $1 ($2; stats: $(cat "$W/stats.json"))"; FAILS=$((FAILS+1)); fi
}

# --- fake camera and broker
"$BIN/xtask" fakecam --addr-file "$W/addr" --stats "$W/stats.json" \
  --main "$ROOT/fixtures/synth/plain_h264.bcm" --sub "$ROOT/fixtures/synth/plain_h264.bcm" 2> "$W/fakecam.log" &
FAKE=$!; track $FAKE
wait_until 10 $FAKE "fake camera address" test -s "$W/addr"
ADDR=$(cat "$W/addr")
mosquitto -p 18883 > "$W/mosquitto.log" 2>&1 &
BROKER=$!; track $BROKER
broker_up() { mosquitto_pub -p 18883 -t neolink/ci/ping -m 1 2>/dev/null; }
wait_until 10 $BROKER "MQTT broker" broker_up

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
NEOLINK=$!; track $NEOLINK

# --- A30: mounts exist before any wake (P16); A23: boot and idle make no contact
wait_until 30 $NEOLINK "neolink start (see $W/neolink.log)" grep -q "fake: Available at.*main" "$W/neolink.log"
check "A30 mounts before any wake" 'grep -q "fake: Available at.*main" "$W/neolink.log"'
# A DESCRIBE-less health probe and an unchanged config re-publish while idle
sleep 3
mosquitto_pub -p 18883 -t neolink/config -m "$(cat "$W/neolink.toml")"
# (the probe's own result does not matter: the check is that it makes no connection)
for _ in 1 2 3 4 5; do curl -s -o /dev/null "http://127.0.0.1:18555/fake/snapshot.jpg" || true; sleep 1; done
check "A23 idle boot: 0 connections" '[ "$(stat connections)" = 0 ]'
check "A23 retained commands ignored" 'grep -q "Ignoring retained MQTT message on neolink/fake/control/wakeup" "$W/neolink.log"'

# --- A24: on-demand cycles; each wakes once and is released with a logout
for cycle in 1 2 3; do
  RC=0
  timeout 30 ffmpeg -v error -nostdin -rtsp_transport tcp -i rtsp://127.0.0.1:18554/fake/main -t 3 -f null - > "$W/ffmpeg$cycle.log" 2>&1 || RC=$?
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
for _ in $(seq 1 20); do curl -s -o "$W/snap.jpg" -D "$W/snap.h" "http://127.0.0.1:18555/fake/snapshot.jpg" || true; done
check "A26 20 snapshots: 0 new connections" '[ "$(stat connections)" = "$BEFORE" ]'
check "A26 snapshot is the camera JPEG" 'grep -q "fake jpeg" "$W/snap.jpg"'
check "A26 Last-Modified" 'grep -qi "^Last-Modified: " "$W/snap.h"'

# --- A29: the CLI reads the state file and makes no connection
BEFORE=$(stat connections)
"$BIN/neolink" battery --config "$W/neolink.toml" fake > "$W/battery.out" 2> "$W/battery.err" || true
check "A29 CLI battery from state" 'grep -q "<batteryPercent>80</batteryPercent>" "$W/battery.out"'
check "A29 CLI battery: 0 connections" '[ "$(stat connections)" = "$BEFORE" ]'
check "A29 second process warned" 'grep -q "Another neolink process" "$W/battery.err"'

# --- Gate 1: no password in logs at the most verbose level a release build has (debug;
# trace is compiled out by release_max_level_debug)
RUST_LOG=trace "$BIN/neolink" battery --fresh --config "$W/neolink.toml" fake > /dev/null 2> "$W/trace.log" || true
check "debug log has content" '[ $(wc -l < "$W/trace.log") -gt 5 ]'
check "no password in trace logs" '! grep -q "fakepass" "$W/trace.log"'

# --- A28: unreachable camera, a query that may wake it (`fresh`; a plain query is
# answered from the state file) arrives: it fails at its deadline, no retry after
kill -USR2 "$FAKE" || die "the fake camera exited during the run"
mosquitto_pub -p 18883 -t neolink/fake/query/pir -m fresh
sleep 2
check "A28 query caused attempts" '[ "$(stat connections)" -gt "$BEFORE" ]'
sleep 65
ATTEMPTS=$(stat connections)
sleep 10
check "A28 no retries after the deadline" '[ "$(stat connections)" = "$ATTEMPTS" ]'

# --- shutdown while streaming: logs out, exits within 5 s (PLAN 1.5, N9)
kill -USR2 "$FAKE" || die "the fake camera exited during the run"   # reachable again
sleep 1
LOGINS=$(stat logins); LOGOUTS=$(stat logouts)
timeout 60 ffmpeg -v error -nostdin -rtsp_transport tcp -i rtsp://127.0.0.1:18554/fake/main -t 30 -f null - > /dev/null 2>&1 &
track $!
i=0; while [ "$(stat logins)" = "$LOGINS" ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done
sleep 2
kill -TERM "$NEOLINK"
i=0; while kill -0 "$NEOLINK" 2>/dev/null && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
check "N9 neolink exits within 5 s of SIGTERM" '! kill -0 "$NEOLINK" 2>/dev/null'
sleep 0.5
check "1.5 SIGTERM logs the camera out" '[ "$(stat logouts)" -gt "$LOGOUTS" ]'
check "1.14 no Thread panicked from the CLI" '! grep -q "Thread panicked" "$W/battery.err"'

echo "logs in $W"
if [ $FAILS = 0 ]; then echo "ALL PASSED"; else echo "$FAILS FAILED"; fi
exit $FAILS
