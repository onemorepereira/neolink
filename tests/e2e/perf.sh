#!/bin/sh
# Performance on fixtures (PLAN 5.7; SPEC N3, N4).
# N3: one 4K H.265 stream (PERF_FIXTURE, e.g. `xtask mkfixture --codec h265 --width 3840
#     --height 2160 --kbps 2400 --irap-kb 600 --p-kb 18`) to 4 TCP clients: server CPU over
#     PERF_SECS (default 120) ≤ 5 % of one core. The Baichuan decryption cost is measured
#     separately on hardware and added (SPEC N3).
# N4: RSS growth per added client ≤ 1 MiB (1 vs 5 clients, 60 s each).
# The server is `xtask rtsp-serve` (the native server alone). Runs in the dev container
# with ffmpeg on PATH after `cargo build --release -p xtask`. E2E_WORK must be fresh.
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
W=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$W"
PORT=${PORT:-48990}
PERF_SECS=${PERF_SECS:-120}
FIX=${PERF_FIXTURE:?set PERF_FIXTURE to a 4K H.265 fixture}
"$BIN/xtask" rtsp-serve --bind 127.0.0.1:$PORT --main "$FIX" --loop --ready-file "$W/ready" \
  > "$W/server.log" 2>&1 &
SRV=$!
i=0; until [ -e "$W/ready" ] || [ $i -gt 100 ]; do sleep 0.1; i=$((i+1)); done
HZ=$(getconf CLK_TCK)
cpu_ticks() { awk '{print $14 + $15}' /proc/$SRV/stat; }
rss() { awk '/VmRSS/ {print $2}' /proc/$SRV/status; }
CLIENTS=""
clients() { # n secs; their pids go in CLIENTS
  CLIENTS=""
  for k in $(seq "$1"); do
    timeout $(($2 + 30)) ffmpeg -hide_banner -loglevel error -nostdin -rtsp_transport tcp \
      -i "rtsp://127.0.0.1:$PORT/argus/main" -t "$2" -c copy -f null - > "$W/client.$1.$k.log" 2>&1 &
    CLIENTS="$CLIENTS $!"
  done
}
wait_clients() { for p in $CLIENTS; do wait "$p"; done; }
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
kill -TERM $SRV; wait $SRV
"$BIN/xtask" rtsp-serve --bind 127.0.0.1:$PORT --main "$FIX" --loop --ready-file "$W/ready2" \
  > "$W/server2.log" 2>&1 &
SRV=$!
i=0; until [ -e "$W/ready2" ] || [ $i -gt 100 ]; do sleep 0.1; i=$((i+1)); done
clients 1 70; sleep 60; r1=$(rss); wait_clients
clients 5 70; sleep 60; r5=$(rss); wait_clients
per=$(awk -v a="$r1" -v b="$r5" 'BEGIN {printf "%.0f", (b - a) / 4}')
msg="RSS $r1 KiB with 1 client, $r5 KiB with 5: ${per} KiB per added client"
if [ "$per" -le 1024 ]; then echo "PASS N4 $msg"; else echo "FAIL N4 $msg"; FAILS=$((FAILS+1)); fi
kill -TERM $SRV; wait $SRV
[ $FAILS = 0 ] && echo "ALL PASSED" || echo "$FAILS FAILED"
exit $FAILS
