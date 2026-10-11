#!/bin/sh
# Frigate's ffmpeg command lines against the native server (PLAN 5.1, 5.2; SPEC A3, A12's
# Frigate tolerance). Frigate itself is not needed: these are its preset input arguments
# and its record/detect outputs.
# - A3: preset-rtsp-generic (record segments + 5 fps detect) and preset-rtsp-restream for
#   A3_SECS (default 900; nightly 5400), on the looped fixture with the camera clock
#   starting 5 min before the u32 wrap. Pass: both run to the end (no reconnect), no
#   error lines, the expected segments.
# - Bridge: a 60 s camera stall at 20 s, with and without audio, BRIDGE_FPS (default
#   0.2): Frigate's -timeout 5000000 must not fire.
# Runs in the dev container with ffmpeg on PATH after `cargo build --release -p xtask`.
# E2E_WORK must be a fresh directory.
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
W=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$W"
F=$ROOT/fixtures/synth
PORT=${PORT:-48950}
A3_SECS=${A3_SECS:-900}
BRIDGE_FPS=${BRIDGE_FPS:-0.2}
TESTS=${TESTS:-A3 bridge}
FAILS=0; N=0
want() { case " $TESTS " in *" $1 "*) return 0;; esac; return 1; }
# Frigate 0.14 presets (frigate/ffmpeg_presets.py)
GENERIC="-avoid_negative_ts make_zero -fflags +genpts+discardcorrupt -rtsp_transport tcp -timeout 5000000 -use_wallclock_as_timestamps 1"
RESTREAM="-rtsp_transport tcp -timeout 5000000"
serve() { # port extra-args...; sets SRV
  p=$1; shift; N=$((N+1))
  "$BIN/xtask" rtsp-serve --bind 127.0.0.1:$p --main "$F/plain_h265.bcm" --sub "$F/plain_h264.bcm" \
    --loop --ready-file "$W/ready.$N" "$@" > "$W/server.$N.log" 2>&1 &
  SRV=$!
  i=0; until [ -e "$W/ready.$N" ] || [ $i -gt 100 ]; do sleep 0.1; i=$((i+1)); done
}
errors() { # log: error lines other than ffmpeg's known startup notes
  # With -use_wallclock_as_timestamps ffmpeg leaves the first packet unset and normalises
  # the next to 0 as well ("Non-monotonic DTS; previous: 0, current: 0; changing to 1"):
  # a one-off at startup that ffmpeg corrects itself. Any other non-monotonic DTS counts.
  grep -i -E "error|invalid|non.monoton|timed out|connection refused|broken pipe" "$1" \
    | grep -v -i -E "deprecated|Timestamps are unset|previous: 0, current: 0; changing to 1" | wc -l
}
frigate_generic() { # url secs name
  mkdir -p "$W/$3.rec"
  # shellcheck disable=SC2086
  timeout $(($2 + 60)) ffmpeg -hide_banner -loglevel warning -nostdin $GENERIC -i "$1" \
    -t "$2" -f segment -segment_time 10 -segment_format mp4 -reset_timestamps 1 -strftime 0 \
      -c:v copy -c:a copy -map 0 "$W/$3.rec/%05d.mp4" \
    -t "$2" -r 5 -vf fps=5,scale=1280:720 -f rawvideo -pix_fmt yuv420p -map 0:v -y /dev/null \
    > "$W/$3.log" 2>&1
}
frigate_restream() { # url secs name
  # shellcheck disable=SC2086
  timeout $(($2 + 60)) ffmpeg -hide_banner -loglevel warning -nostdin $RESTREAM -i "$1" \
    -t "$2" -c copy -f null - > "$W/$3.log" 2>&1
}

if want A3; then
  serve $PORT --wrap-in 300
  url=rtsp://127.0.0.1:$PORT/argus/main
  t0=$(date +%s)
  frigate_generic "$url" "$A3_SECS" a3.generic & G=$!
  frigate_restream "$url" "$A3_SECS" a3.restream & R=$!
  wait $G; gx=$?; wait $R; rx=$?
  took=$(( $(date +%s) - t0 ))
  segs=$(ls "$W/a3.generic.rec" | wc -l)
  ge=$(errors "$W/a3.generic.log"); re=$(errors "$W/a3.restream.log")
  minsegs=$(( A3_SECS / 10 - 2 ))
  msg="${A3_SECS}s across the wrap: generic exit=$gx errors=$ge segments=$segs (min $minsegs); restream exit=$rx errors=$re; took ${took}s"
  if [ $gx = 0 ] && [ $rx = 0 ] && [ "$ge" = 0 ] && [ "$re" = 0 ] && [ "$segs" -ge $minsegs ] && [ $took -le $((A3_SECS + 30)) ]; then
    echo "PASS A3 $msg"
  else
    echo "FAIL A3 $msg"; sed 's/^/    /' "$W/a3.generic.log" | head -10; FAILS=$((FAILS+1))
  fi
  kill -TERM $SRV; wait $SRV
fi

if want bridge; then
  for audio in with no; do
    extra=""; [ $audio = no ] && extra="--no-audio"
    # shellcheck disable=SC2086
    serve $((PORT+1)) --stall-at 20 --stall-for 60 --bridge-fps "$BRIDGE_FPS" $extra
    url=rtsp://127.0.0.1:$((PORT+1))/argus/main
    frigate_generic "$url" 100 bridge.$audio; gx=$?
    segs=$(ls "$W/bridge.$audio.rec" | wc -l); ge=$(errors "$W/bridge.$audio.log")
    msg="60 s stall, $audio audio, bridge_fps=$BRIDGE_FPS: exit=$gx errors=$ge segments=$segs"
    if [ $gx = 0 ] && [ "$ge" = 0 ] && [ "$segs" -ge 8 ]; then echo "PASS bridge $msg"
    else echo "FAIL bridge $msg"; sed 's/^/    /' "$W/bridge.$audio.log" | head -10; FAILS=$((FAILS+1)); fi
    kill -TERM $SRV; wait $SRV
  done
fi
[ $FAILS = 0 ] && echo "ALL PASSED" || echo "$FAILS FAILED"
exit $FAILS
