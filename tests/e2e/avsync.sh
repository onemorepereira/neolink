#!/usr/bin/env bash
# A16: A/V sync through the native server. Records the looping flash/beep fixture with
# ffmpeg (-c copy, so ffmpeg's own RTCP-based stream alignment applies, as in clients) and
# measures |audio − video| with `xtask avsync`. Durations: AV_SECS (default "5 60"; the
# full gate adds 900). Main and sub are recorded at the same time, so a run takes
# sum(AV_SECS) plus start-up. Runs in the dev container after `cargo build --release -p xtask`.
# E2E_WORK must be a fresh directory.
# Exit status: the number of failed recordings; 1 when the run itself broke.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
W=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$W"
F=$ROOT/fixtures/synth
PORT=${PORT:-48664}
FAKE=""
if [ "${SERVER:-rtsp-serve}" = neolink ]; then
  # Gate 4: `neolink rtsp` on the native backend against `xtask fakecam`
  "$BIN/xtask" fakecam --addr-file "$W/fake" --stats "$W/fakestats.json" \
    --main "$F/plain_h265.bcm" --sub "$F/plain_h264.bcm" > "$W/fake.log" 2>&1 &
  FAKE=$!; track $FAKE
  wait_until 10 $FAKE "fake camera address" test -s "$W/fake"
  printf 'bind = "127.0.0.1"\nbind_port = %s\nsnapshot_port = 0\nrtsp_backend = "native"\n[[cameras]]\nname = "argus"\nusername = "admin"\npassword = "fakepass"\naddress = "%s"\n' \
    "$PORT" "$(cat "$W/fake")" > "$W/neolink.toml"
  NEOLINK_STATE_DIR="$W/state" RUST_LOG=info "$BIN/neolink" rtsp --config "$W/neolink.toml" > "$W/server.log" 2>&1 &
  SRV=$!; track $SRV
  wait_until 30 $SRV "neolink start (see $W/server.log)" grep -q "Starting RTSP Server (native)" "$W/server.log"
else
  "$BIN/xtask" rtsp-serve --bind 127.0.0.1:"$PORT" --main "$F/plain_h265.bcm" --sub "$F/plain_h264.bcm" \
    --loop --ready-file "$W/ready" > "$W/server.log" 2>&1 &
  SRV=$!; track $SRV
  wait_until 10 $SRV "rtsp-serve start (see $W/server.log)" test -e "$W/ready"
fi
FAILS=0
record() { # path secs
  timeout $(($2 + 60)) ffmpeg -v error -nostdin -rtsp_transport tcp -i "rtsp://127.0.0.1:$PORT/argus/$1" \
    -t "$2" -c copy "$W/${1}_$2.mkv" > "$W/${1}_$2.ffmpeg.log" 2>&1
}
check() { # path secs ffmpeg-exit
  local path=$1 secs=$2 rc=$3 out="$W/${1}_$2.mkv" starts min
  if [ "$rc" != 0 ] || [ ! -s "$out" ]; then
    echo "FAIL A16 $path ${secs}s: ffmpeg exit $rc"; sed 's/^/    /' "$W/${path}_$secs.ffmpeg.log" | tail -5
    FAILS=$((FAILS+1)); return
  fi
  starts=$(ffprobe -v error -show_entries stream=codec_type,start_time -of csv=p=0 "$out" | tr '\n' ' ')
  # A 5 s recording holds two flashes, the first possibly before audio starts
  min=$(( secs / 2 - 1 )); [ $min -lt 1 ] && min=1
  if "$BIN/xtask" avsync --min-pairs $min "$out" > "$W/${path}_$secs.avsync" 2>&1; then
    echo "PASS A16 $path ${secs}s: $(tail -1 "$W/${path}_$secs.avsync"); start times: $starts"
  else
    echo "FAIL A16 $path ${secs}s:"; sed 's/^/    /' "$W/${path}_$secs.avsync"; FAILS=$((FAILS+1))
  fi
}
for secs in ${AV_SECS:-5 60}; do
  alive "$SRV" server
  if [ -n "$FAKE" ]; then alive "$FAKE" "fake camera"; fi
  record main "$secs" & M=$!; track $M
  record sub "$secs" & S=$!; track $S
  mx=0; wait_pid $M $((secs + 90)) "main ${secs}s recording" || mx=$?
  sx=0; wait_pid $S $((secs + 90)) "sub ${secs}s recording" || sx=$?
  check main "$secs" $mx
  check sub "$secs" $sx
done
stop_pid $SRV server
if [ -n "$FAKE" ]; then stop_pid $FAKE "fake camera"; fi
if [ $FAILS = 0 ]; then echo "ALL PASSED"; else echo "$FAILS FAILED"; fi
exit $FAILS
