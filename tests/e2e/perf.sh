#!/usr/bin/env bash
# Performance on fixtures (PLAN 5.7; SPEC N3, N4).
# N3: one 4K H.265 stream (PERF_FIXTURE, e.g. `xtask mkfixture --codec h265 --width 3840
#     --height 2160 --kbps 2400 --irap-kb 600 --p-kb 18`) to 4 TCP clients: server CPU over
#     PERF_SECS (default 120) ≤ 5 % of one core. The Baichuan decryption cost is measured
#     separately on hardware and added (SPEC N3).
# N4: RSS growth per added client ≤ 1 MiB (1 vs 5 clients, 60 s each).
# The server is `xtask rtsp-serve` (the native server alone). Runs in the dev container
# with ffmpeg on PATH after `cargo build --release -p xtask`. E2E_WORK must be fresh.
# Exit status: the number of failed checks; 1 when the run itself broke.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
W=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$W"
PORT=${PORT:-48990}
PERF_SECS=${PERF_SECS:-120}
FIX=${PERF_FIXTURE:?set PERF_FIXTURE to a 4K H.265 fixture}
"$BIN/xtask" rtsp-serve --bind 127.0.0.1:"$PORT" --main "$FIX" --loop --ready-file "$W/ready" \
  > "$W/server.log" 2>&1 &
SRV=$!; track $SRV
wait_until 10 $SRV "rtsp-serve start (see $W/server.log)" test -e "$W/ready"
HZ=$(getconf CLK_TCK)
cpu_ticks() { [ -r "/proc/$SRV/stat" ] || die "rtsp-serve exited during the run"; awk '{print $14 + $15}' "/proc/$SRV/stat"; }
rss() { [ -r "/proc/$SRV/status" ] || die "rtsp-serve exited during the run"; awk '/VmRSS/ {print $2}' "/proc/$SRV/status"; }
CLIENTS=""
clients() { # n secs; their pids go in CLIENTS
  CLIENTS=""
  for k in $(seq "$1"); do
    timeout $(($2 + 30)) ffmpeg -hide_banner -loglevel error -nostdin -rtsp_transport tcp \
      -i "rtsp://127.0.0.1:$PORT/argus/main" -t "$2" -c copy -f null - > "$W/client.$1.$k.log" 2>&1 &
    CLIENTS="$CLIENTS $!"; track $!
  done
}
# Every client must play to the end: a client that failed makes the numbers meaningless
wait_clients() {
  local p rc
  for p in $CLIENTS; do
    rc=0; wait_pid "$p" $((PERF_SECS + 120)) "perf client" || rc=$?
    [ $rc = 0 ] || die "a perf client exited $rc (see $W/client.*.log)"
  done
}
FAILS=0
# N3: 4 clients; measure after a 10 s warm-up
clients 4 $((PERF_SECS + 15))
sleep 10
a=$(cpu_ticks); sleep "$PERF_SECS"; b=$(cpu_ticks)
wait_clients
pct=$(awk -v t=$((b - a)) -v hz="$HZ" -v s="$PERF_SECS" 'BEGIN {printf "%.2f", t * 100 / (hz * s)}')
ok=$(awk -v p="$pct" 'BEGIN {print (p <= 5) ? 1 : 0}')
msg="4K H.265 to 4 clients: ${pct} % of one core over ${PERF_SECS}s"
if [ "$ok" = 1 ]; then echo "PASS N3 $msg"; else echo "FAIL N3 $msg"; FAILS=$((FAILS+1)); fi
# N4: RSS with 1, then 5 clients, on a fresh server (N3's high-water mark would
# otherwise be in the 1-client baseline)
stop_pid $SRV rtsp-serve
"$BIN/xtask" rtsp-serve --bind 127.0.0.1:"$PORT" --main "$FIX" --loop --ready-file "$W/ready2" \
  > "$W/server2.log" 2>&1 &
SRV=$!; track $SRV
wait_until 10 $SRV "rtsp-serve start (see $W/server2.log)" test -e "$W/ready2"
clients 1 70; sleep 60; r1=$(rss); wait_clients
clients 5 70; sleep 60; r5=$(rss); wait_clients
per=$(awk -v a="$r1" -v b="$r5" 'BEGIN {printf "%.0f", (b - a) / 4}')
msg="RSS $r1 KiB with 1 client, $r5 KiB with 5: ${per} KiB per added client"
if [ "$per" -le 1024 ]; then echo "PASS N4 $msg"; else echo "FAIL N4 $msg"; FAILS=$((FAILS+1)); fi
stop_pid $SRV rtsp-serve
if [ $FAILS = 0 ]; then echo "ALL PASSED"; else echo "$FAILS FAILED"; fi
exit $FAILS
