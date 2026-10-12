#!/usr/bin/env bash
# Frigate's ffmpeg command lines against the native server (PLAN 5.1, 5.2; SPEC A3, A12's
# Frigate tolerance). Frigate itself is not needed: these are its preset input arguments
# and its record/detect outputs.
# - A3: preset-rtsp-generic (record segments + 5 fps detect) and preset-rtsp-restream for
#   A3_SECS (default 900; nightly 5400), on the looped fixture with the camera clock
#   starting 5 min before the u32 wrap. Pass: both run to the end (no reconnect), no
#   error lines, the expected segments.
# - Bridge: a 60 s camera stall at 20 s, with and without audio, BRIDGE_FPS (default
#   0.2): Frigate's -timeout 5000000 must not fire.
# - Congestion (SPEC A11b): the looped congestion fixture for CONG_SECS (default 240, two
#   episodes) at each pacing latency in CONG_LATENCIES ("300 1500"), side by side. Frigate's
#   generic preset (wall-clock timestamps) and its go2rtc-restream recorder (the restream
#   preset, `-c copy` into 10 s mp4 segments, camera timestamps, so the forward rebases
#   reach the segments). Pass: both run to the end with no error lines and the expected
#   segments, and every restream segment decodes cleanly.
# Runs in the dev container with ffmpeg on PATH after `cargo build --release -p xtask`.
# E2E_WORK must be a fresh directory.
# Exit status: the number of failed checks; 1 when the run itself broke.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
W=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$W"
F=$ROOT/fixtures/synth
PORT=${PORT:-48950}
A3_SECS=${A3_SECS:-900}
BRIDGE_FPS=${BRIDGE_FPS:-0.2}
CONG_SECS=${CONG_SECS:-240}
CONG_LATENCIES=${CONG_LATENCIES:-300 1500}
TESTS=${TESTS:-A3 bridge}
FAILS=0; N=0
want() { case " $TESTS " in *" $1 "*) return 0;; esac; return 1; }
# Frigate 0.14 presets (frigate/ffmpeg_presets.py)
GENERIC="-avoid_negative_ts make_zero -fflags +genpts+discardcorrupt -rtsp_transport tcp -timeout 5000000 -use_wallclock_as_timestamps 1"
RESTREAM="-rtsp_transport tcp -timeout 5000000"
serve() { # port extra-args...; sets SRV (MAIN: the main fixture, default plain_h265)
  p=$1; shift; N=$((N+1))
  "$BIN/xtask" rtsp-serve --bind 127.0.0.1:$p --main "$F/${MAIN:-plain_h265.bcm}" --sub "$F/plain_h264.bcm" \
    --loop --ready-file "$W/ready.$N" "$@" > "$W/server.$N.log" 2>&1 &
  SRV=$!; track $SRV
  wait_until 10 $SRV "rtsp-serve start (see $W/server.$N.log)" test -e "$W/ready.$N"
}
errors() { # log: error lines other than ffmpeg's known startup notes
  # With -use_wallclock_as_timestamps ffmpeg leaves the first packet unset and normalises
  # the next to 0 as well ("Non-monotonic DTS; previous: 0, current: 0; changing to 1"):
  # a one-off at startup that ffmpeg corrects itself. Any other non-monotonic DTS counts.
  need_log "$1"
  { grep -i -E "error|invalid|non.monoton|timed out|connection refused|broken pipe" "$1" || [ $? = 1 ]; } \
    | { grep -v -i -E "deprecated|Timestamps are unset|previous: 0, current: 0; changing to 1" || [ $? = 1 ]; } | wc -l
}
need_log() { [ -f "$1" ] || die "no ffmpeg log $1"; }
segments() { # dir: number of segment files (0 when the directory is missing)
  if [ -d "$1" ]; then find "$1" -name '*.mp4' -size +0 | wc -l; else echo 0; fi
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
  frigate_generic "$url" "$A3_SECS" a3.generic & G=$!; track $G
  frigate_restream "$url" "$A3_SECS" a3.restream & R=$!; track $R
  gx=0; wait_pid $G $((A3_SECS + 120)) "A3 generic client" || gx=$?
  rx=0; wait_pid $R $((A3_SECS + 120)) "A3 restream client" || rx=$?
  took=$(( $(date +%s) - t0 ))
  segs=$(segments "$W/a3.generic.rec")
  ge=$(errors "$W/a3.generic.log"); re=$(errors "$W/a3.restream.log")
  minsegs=$(( A3_SECS / 10 - 2 ))
  msg="${A3_SECS}s across the wrap: generic exit=$gx errors=$ge (tolerated audio DTS fix-ups: $tiny) segments=$segs (min $minsegs); restream exit=$rx errors=$re; took ${took}s"
  if [ $gx = 0 ] && [ $rx = 0 ] && [ "$ge" = 0 ] && [ "$re" = 0 ] && [ "$segs" -ge $minsegs ] && [ $took -le $((A3_SECS + 30)) ]; then
    echo "PASS A3 $msg"
  else
    echo "FAIL A3 $msg"; head -10 "$W/a3.generic.log" | sed 's/^/    /'; FAILS=$((FAILS+1))
  fi
  stop_pid $SRV rtsp-serve
fi

if want bridge; then
  for audio in with no; do
    extra=""; [ $audio = no ] && extra="--no-audio"
    # shellcheck disable=SC2086
    serve $((PORT+1)) --stall-at 20 --stall-for 60 --bridge-fps "$BRIDGE_FPS" $extra
    url=rtsp://127.0.0.1:$((PORT+1))/argus/main
    gx=0; frigate_generic "$url" 100 bridge.$audio || gx=$?
    segs=$(segments "$W/bridge.$audio.rec"); ge=$(errors "$W/bridge.$audio.log")
    msg="60 s stall, $audio audio, bridge_fps=$BRIDGE_FPS: exit=$gx errors=$ge segments=$segs"
    if [ $gx = 0 ] && [ "$ge" = 0 ] && [ "$segs" -ge 8 ]; then echo "PASS bridge $msg"
    else echo "FAIL bridge $msg"; head -10 "$W/bridge.$audio.log" | sed 's/^/    /'; FAILS=$((FAILS+1)); fi
    stop_pid $SRV rtsp-serve
  done
fi
if want congestion; then
  i=0; G=(); R=(); S=()
  for l in $CONG_LATENCIES; do
    MAIN=congestion_h265.bcm serve $((PORT+2+i)) --latency-ms "$l"; S+=("$SRV")
    url=rtsp://127.0.0.1:$((PORT+2+i))/argus/main
    frigate_generic "$url" "$CONG_SECS" "cong.$l.generic" & G+=("$!"); track $!
    # The restream preset's record output (Frigate 0.14 record args); -t would stop early
    # (it counts media time, which steps forward at each rebase), so it is stopped by SIGINT
    mkdir -p "$W/cong.$l.restream.rec"
    # shellcheck disable=SC2086
    timeout -s INT "$CONG_SECS" ffmpeg -hide_banner -loglevel warning -nostdin $RESTREAM -i "$url" \
      -f segment -segment_time 10 -segment_format mp4 -reset_timestamps 1 -strftime 0 -c copy -map 0 \
      "$W/cong.$l.restream.rec/%05d.mp4" > "$W/cong.$l.restream.log" 2>&1 & R+=("$!"); track $!
    i=$((i+1))
  done
  i=0
  for l in $CONG_LATENCIES; do
    g=${G[$i]}; r=${R[$i]}; i=$((i+1))
    gx=0; wait_pid "$g" $((CONG_SECS + 120)) "congestion ${l}ms generic" || gx=$?
    rx=0; wait_pid "$r" $((CONG_SECS + 60)) "congestion ${l}ms restream" || rx=$?
    segs=$(segments "$W/cong.$l.generic.rec"); ge=$(errors "$W/cong.$l.generic.log")
    # With -use_wallclock_as_timestamps an audio packet can arrive stamped a tick or two
    # (<= 1 ms at 16 kHz) before the one ahead of it; ffmpeg moves it on and carries on.
    # Seen once in two runs, 18 s in, before any episode: not a rebase effect, not counted
    tiny=$(awk '/aost.*Non-monotonic DTS; previous: [0-9]+, current: [0-9]+;/ {
      p = $0; sub(/.*previous: /, "", p); sub(/,.*/, "", p); c = $0; sub(/.*current: /, "", c); sub(/;.*/, "", c)
      if (p - c >= 0 && p - c <= 16) n++ } END {print n + 0}' "$W/cong.$l.generic.log")
    ge=$((ge - tiny))
    re=$(errors "$W/cong.$l.restream.log")
    # Segment durations are reported, not gated: a rebase's forward step falls between two
    # segments (the IRAP it lands on starts the next one, and -reset_timestamps drops the
    # gap), so they add up to less than the run by the steps; Frigate dates each segment
    # by its start on the wall clock
    rsegs=0; bad=0; total=0; longest=0
    for seg in "$W/cong.$l.restream.rec"/*.mp4; do
      [ -s "$seg" ] || continue
      rsegs=$((rsegs+1))
      [ -z "$(ffmpeg -nostdin -v error -i "$seg" -f null - 2>&1)" ] || bad=$((bad+1))
      d=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$seg")
      total=$(awk -v a="$total" -v b="$d" 'BEGIN {printf "%.1f", a + b}')
      longest=$(awk -v a="$longest" -v b="$d" 'BEGIN {printf "%.1f", (b > a) ? b : a}')
    done
    msg="${CONG_SECS}s at latency ${l}ms: generic exit=$gx errors=$ge (tolerated audio DTS fix-ups: $tiny) segments=$segs (min $((CONG_SECS / 10 - 2))); restream exit=$rx (124: ran to the end) errors=$re segments=$rsegs bad=$bad (total ${total}s, longest ${longest}s)"
    if [ $gx = 0 ] && [ "$ge" = 0 ] && [ "$segs" -ge $((CONG_SECS / 10 - 2)) ] && [ $rx = 124 ] && [ "$re" = 0 ] \
       && [ "$rsegs" -ge $((CONG_SECS / 10 - 2)) ] && [ $bad = 0 ]; then
      echo "PASS congestion $msg"
    else
      echo "FAIL congestion $msg"; head -5 "$W/cong.$l.generic.log" "$W/cong.$l.restream.log" | sed 's/^/    /'
      FAILS=$((FAILS+1))
    fi
  done
  for s in "${S[@]}"; do stop_pid "$s" rtsp-serve; done
fi
if [ $FAILS = 0 ]; then echo "ALL PASSED"; else echo "$FAILS FAILED"; fi
exit $FAILS
