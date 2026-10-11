#!/bin/sh
# A16: A/V sync through the native server. Records the looping flash/beep fixture with
# ffmpeg (-c copy, so ffmpeg's own RTCP-based stream alignment applies, as in clients) and
# measures |audio − video| with `xtask avsync`. Durations: AV_SECS (default "5 60"; the
# full gate adds 900). Runs in the dev container after `cargo build --release -p xtask`.
# E2E_WORK must be a fresh directory.
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
W=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$W"
F=$ROOT/fixtures/synth
PORT=${PORT:-48664}
"$BIN/xtask" rtsp-serve --bind 127.0.0.1:$PORT --main "$F/plain_h265.bcm" --sub "$F/plain_h264.bcm" \
  --loop --ready-file "$W/ready" > "$W/server.log" 2>&1 &
SRV=$!
i=0; until [ -e "$W/ready" ] || [ $i -gt 100 ]; do sleep 0.1; i=$((i+1)); done
FAILS=0
for secs in ${AV_SECS:-5 60}; do
  for path in main sub; do
    out="$W/${path}_$secs.mkv"
    timeout $((secs + 60)) ffmpeg -v error -nostdin -rtsp_transport tcp -i "rtsp://127.0.0.1:$PORT/argus/$path" \
      -t "$secs" -c copy "$out" > "$W/${path}_$secs.ffmpeg.log" 2>&1
    starts=$(ffprobe -v error -show_entries stream=codec_type,start_time -of csv=p=0 "$out" | tr '\n' ' ')
    # A 5 s recording holds two flashes, the first possibly before audio starts
    min=$(( secs / 2 - 1 )); [ $min -lt 1 ] && min=1
    if "$BIN/xtask" avsync --min-pairs $min "$out" > "$W/${path}_$secs.avsync" 2>&1; then
      echo "PASS A16 $path ${secs}s: $(tail -1 "$W/${path}_$secs.avsync"); start times: $starts"
    else
      echo "FAIL A16 $path ${secs}s:"; sed 's/^/    /' "$W/${path}_$secs.avsync"; FAILS=$((FAILS+1))
    fi
  done
done
kill -TERM $SRV; wait $SRV
[ $FAILS = 0 ] && echo "ALL PASSED" || echo "$FAILS FAILED"
exit $FAILS
