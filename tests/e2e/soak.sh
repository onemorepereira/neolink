#!/bin/sh
# Soak (PLAN 5.6, SPEC A15/N5 surrogate on fixtures): `neolink rtsp` (native) against
# `xtask fakecam` replaying the looped fixtures (a camera-clock discontinuity every 20 s
# lap), two ffmpeg clients (main and sub) for SOAK_SECS (default 3600; nightly 86400).
# Pass: both clients run to the end (no reconnect), no timestamp warnings, and neolink's
# RSS grows ≤ 10 % from the end of the first hour (or the first quarter, for runs under
# 2 h) to the end. Needs neolink built with --features native-rtsp.
# E2E_WORK must be a fresh directory.
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
W=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$W"
F=$ROOT/fixtures/synth
PORT=${PORT:-48980}
SOAK_SECS=${SOAK_SECS:-3600}
"$BIN/xtask" fakecam --addr-file "$W/fake" --stats "$W/fakestats.json" \
  --main "$F/plain_h265.bcm" --sub "$F/plain_h264.bcm" > "$W/fake.log" 2>&1 &
FAKE=$!
i=0; until [ -s "$W/fake" ] || [ $i -gt 100 ]; do sleep 0.1; i=$((i+1)); done
printf 'bind = "127.0.0.1"\nbind_port = %s\nsnapshot_port = 0\nrtsp_backend = "native"\n[[cameras]]\nname = "argus"\nusername = "admin"\npassword = "fakepass"\naddress = "%s"\n' \
  $PORT "$(cat "$W/fake")" > "$W/neolink.toml"
NEOLINK_STATE_DIR="$W/state" RUST_LOG=info "$BIN/neolink" rtsp --config "$W/neolink.toml" > "$W/neolink.log" 2>&1 &
NL=$!
i=0; until grep -q "Starting RTSP Server (native)" "$W/neolink.log" || [ $i -gt 300 ]; do sleep 0.1; i=$((i+1)); done
rss() { awk '/VmRSS/ {print $2}' /proc/$NL/status; } # KiB
client() { # path
  timeout $((SOAK_SECS + 120)) ffmpeg -hide_banner -loglevel warning -nostdin -rtsp_transport tcp \
    -i "rtsp://127.0.0.1:$PORT/argus/$1" -t "$SOAK_SECS" -c copy -f null - > "$W/client.$1.log" 2>&1
  echo $? > "$W/client.$1.exit"
}
client main & C1=$!
client sub & C2=$!
mark=$(( SOAK_SECS >= 7200 ? 3600 : SOAK_SECS / 4 ))
t0=$(date +%s); RSS_MARK=""
while kill -0 $C1 2>/dev/null || kill -0 $C2 2>/dev/null; do
  now=$(( $(date +%s) - t0 ))
  echo "$now $(rss)" >> "$W/rss.txt"
  if [ -z "$RSS_MARK" ] && [ $now -ge $mark ]; then RSS_MARK=$(rss); fi
  sleep 30
done
RSS_END=$(rss)
kill -TERM $NL; wait $NL; kill -TERM $FAKE; wait $FAKE
FAILS=0
for p in main sub; do
  ex=$(cat "$W/client.$p.exit")
  warn=$(grep -c -i -E "non.monoton|discontinu|error|timed out|invalid" "$W/client.$p.log" || true)
  if [ "$ex" = 0 ] && [ "$warn" = 0 ]; then echo "PASS soak client $p: ${SOAK_SECS}s, exit 0, no warnings"
  else echo "FAIL soak client $p: exit=$ex warnings=$warn"; head -5 "$W/client.$p.log" | sed 's/^/    /'; FAILS=$((FAILS+1)); fi
done
growth=$(awk -v a="${RSS_MARK:-0}" -v b="$RSS_END" 'BEGIN {if (a>0) printf "%.1f", (b-a)*100/a; else print 999}')
ok=$(awk -v g="$growth" 'BEGIN {print (g <= 10) ? 1 : 0}')
msg="RSS ${RSS_MARK:-?} KiB at ${mark}s, $RSS_END KiB at the end (${growth} %)"
if [ "$ok" = 1 ]; then echo "PASS soak memory: $msg"; else echo "FAIL soak memory: $msg"; FAILS=$((FAILS+1)); fi
[ $FAILS = 0 ] && echo "ALL PASSED" || echo "$FAILS FAILED"
exit $FAILS
