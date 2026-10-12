#!/bin/sh
# Gate 3 client acceptance (SPEC §7: A1, A2, A2b, A4, A4b, A5, A10, A11b; PLAN 3.9/3.11) against
# the native server (xtask rtsp-serve) on the synthetic fixtures, with real clients:
# ffprobe/ffmpeg, go2rtc and VLC (cvlc).
# Runs in the neolink-clients image (tests/e2e/clients/Containerfile) with --network host,
# the pinned static ffmpeg on PATH (/ffmpeg/bin) and a built xtask:
#   cargo build --release -p xtask && sh tests/e2e/clients.sh
# Exit status: the number of failed checks; 2 when a server or go2rtc did not start.
# E2E_WORK must be a fresh directory (nothing in it is deleted or reused).
# Env: TESTS (default "A1 A2 A2b A4 A4b A5 A10"), A2_SECS (120), A2B_SECS (20),
#      A5_SECS (60; the full gate is 300), A11B_SECS (240: two congestion episodes),
#      A11B_LATENCIES (pacing latencies, ms, run side by side; at most 2, default "300 1500"),
#      A11B_VLC_STALL (VLC's longest allowed display freeze, s, default 18),
#      PSETS (the --parameter-sets mode for the main server, default the server's own), A4B_MODES ("strip passthrough inject"),
#      A4B_GATE (the A4b modes that must pass, default "strip"),
#      PORT (base port, default 48710; uses PORT..PORT+29),
#      SERVER (rtsp-serve, the default, or neolink: `neolink rtsp` on the native backend
#      against `xtask fakecam`, which needs neolink built with --features native-rtsp;
#      Gate 4 re-runs the matrix this way), NEOLINK_EXTRA (extra camera config lines
#      with SERVER=neolink).
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
W=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$W"
F=$ROOT/fixtures/synth
TESTS=${TESTS:-A1 A2 A2b A4 A4b A5 A10}
A2_SECS=${A2_SECS:-120}
A2B_SECS=${A2B_SECS:-20}
A5_SECS=${A5_SECS:-60}
A11B_SECS=${A11B_SECS:-240}
A11B_LATENCIES=${A11B_LATENCIES:-300 1500}
A11B_VLC_STALL=${A11B_VLC_STALL:-18}
PSETS=${PSETS:-}
A4B_MODES=${A4B_MODES:-strip passthrough inject}
# Modes whose A4b result gates the run (the default); the others are reported as INFO
A4B_GATE=${A4B_GATE:-strip}
PORT=${PORT:-48710}
G2R_RTSP=$((PORT+1)); G2R_API=$((PORT+2)); VLC_RC=$((PORT+3))
# What the fixtures carry (fixtures/synth, `xtask mkfixture --standard`)
FPS=15
MAIN_CODEC=hevc; MAIN_RES=640x360
SUB_CODEC=h264;  SUB_RES=640x360
FFMPEG=${FFMPEG:-ffmpeg}; FFPROBE=${FFPROBE:-ffprobe}
FAILS=0; PIDS=""

pass() { echo "PASS $*"; }
info() { echo "INFO $*"; }
fail() { echo "FAIL $*"; FAILS=$((FAILS+1)); }
want() { case " $TESTS " in *" $1 "*) return 0;; esac; return 1; }
wait_for() { # file [tenths]
  i=0; until [ -e "$1" ] || [ $i -gt "${2:-100}" ]; do sleep 0.1; i=$((i+1)); done
  [ -e "$1" ]
}
a11b_idx() { # latency -> its slot (0 or 1) among A11B_LATENCIES
  n=0; for x in $A11B_LATENCIES; do [ "$x" = "$1" ] && { echo $n; return; }; n=$((n+1)); done; echo 0
}
cleanup() { for p in $PIDS; do kill -TERM "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT
trap 'exit 130' INT TERM

# The rtsp-serve options as a neolink config: --user, --permit, --auth, --parameter-sets
serve_neolink() { # port name extra-args...; sets SRV
  p=$1; n=$2; shift 2
  users=""; permit=""; auth=""; psets=""
  while [ $# -gt 0 ]; do
    case $1 in
      --user) users="$users
[[users]]
name = \"${2%%:*}\"
pass = \"${2#*:}\""; shift 2;;
      --permit) permit="$permit\"$2\", "; shift 2;;
      --auth) auth="rtsp_auth = [\"$2\"]"; shift 2;;
      --parameter-sets) psets="parameter_sets = \"$2\""; shift 2;;
      *) echo "serve_neolink: unknown option $1"; exit 2;;
    esac
  done
  "$BIN/xtask" fakecam --addr-file "$W/fake.$n" --stats "$W/fakestats.$n.json" \
    --main "$F/plain_h265.bcm" --sub "$F/plain_h264.bcm" > "$W/fake.$n.log" 2>&1 &
  PIDS="$PIDS $!"
  wait_for "$W/fake.$n" || { echo "fakecam $n did not start"; exit 2; }
  {
    printf 'bind = "127.0.0.1"\nbind_port = %s\nsnapshot_port = 0\nrtsp_backend = "native"\n%s\n' "$p" "$auth"
    printf '[[cameras]]\nname = "argus"\nusername = "admin"\npassword = "fakepass"\naddress = "%s"\n%s\n%s\n' \
      "$(cat "$W/fake.$n")" "$psets" "${NEOLINK_EXTRA:-}"
    [ -n "$permit" ] && printf 'permitted_users = [%s]\n' "${permit%, }"
    printf '%s\n' "$users"
  } > "$W/neolink.$n.toml"
  NEOLINK_STATE_DIR="$W/state.$n" RUST_LOG=info "$BIN/neolink" rtsp --config "$W/neolink.$n.toml" \
    > "$W/server.$n.log" 2>&1 &
  SRV=$!; PIDS="$PIDS $SRV"
  i=0; until grep -q "Starting RTSP Server (native)" "$W/server.$n.log" || [ $i -gt 300 ]; do sleep 0.1; i=$((i+1)); done
  grep -q "Starting RTSP Server (native)" "$W/server.$n.log" || { echo "neolink $n did not start:"; cat "$W/server.$n.log"; exit 2; }
}

serve() { # port name extra-args...; sets SRV
  if [ "${SERVER:-rtsp-serve}" = neolink ]; then serve_neolink "$@"; return; fi
  p=$1; n=$2; shift 2
  "$BIN/xtask" rtsp-serve --bind 127.0.0.1:$p --main "$F/plain_h265.bcm" --sub "$F/plain_h264.bcm" \
    --loop --ready-file "$W/ready.$n" --stats "$W/stats.$n.json" "$@" > "$W/server.$n.log" 2>&1 &
  SRV=$!; PIDS="$PIDS $SRV"
  wait_for "$W/ready.$n" || { echo "server $n did not start:"; cat "$W/server.$n.log"; exit 2; }
}
stop() { kill -TERM "$1" 2>/dev/null; wait "$1" 2>/dev/null; }
# A server must still be running when its tests end: one that died mid-run fails the run
stop_server() { # pid name
  if ! kill -0 "$1" 2>/dev/null; then fail "$2: the server exited during the run"; return; fi
  stop "$1"
}
url() { echo "rtsp://${2:-}127.0.0.1:$PORT/argus/$1"; }
count() { grep -c -E "$1" "$2" 2>/dev/null || true; } # pattern file -> number of lines

MAINARGS=""; [ -n "$PSETS" ] && MAINARGS="--parameter-sets $PSETS"
# shellcheck disable=SC2086
serve $PORT main $MAINARGS
MAIN_SRV=$SRV

# ---- A1: ffprobe over TCP: codec, resolution, fps, audio; the first frame decodes --------
a1() { # stream codec res
  s=$1; L=$W/a1.$s
  timeout 30 "$FFPROBE" -v error -rtsp_transport tcp -show_entries stream=codec_type,codec_name,width,height \
    -of compact=p=0 "$(url "$s")" > "$L.streams" 2> "$L.err"
  v=$(grep 'codec_type=video' "$L.streams" | head -1)
  codec=$(echo "$v" | sed -n 's/.*codec_name=\([^|]*\).*/\1/p')
  res=$(echo "$v" | sed -n 's/.*width=\([0-9]*\)|height=\([0-9]*\).*/\1x\2/p')
  audio=$(grep 'codec_type=audio' "$L.streams" | sed -n 's/.*codec_name=\([^|]*\).*/\1/p')
  # fps from 20 s of video packet timestamps (decode order is presentation order here)
  timeout 60 "$FFPROBE" -v error -rtsp_transport tcp -select_streams v:0 -read_intervals %+20 \
    -show_entries packet=pts_time -of csv=p=0 "$(url "$s")" > "$L.pts" 2>> "$L.err"
  fps=$(awk -F, '$1!="" && $1!="N/A" {n++; if (n==1) f=$1; l=$1} END {if (n>1) printf "%.3f", (n-1)/(l-f); else print 0}' "$L.pts")
  fpsok=$(awk -v f="$fps" -v e=$FPS 'BEGIN {print (f >= e*0.95 && f <= e*1.05) ? 1 : 0}')
  # A7-style: the first frame decodes with no errors (under strip only the SDP has the parameter sets)
  timeout 30 "$FFMPEG" -nostdin -v error -rtsp_transport tcp -i "$(url "$s")" -map 0:v -frames:v 1 \
    -f null - > "$L.first" 2>&1
  first=$?
  msg="$s: codec=$codec res=$res fps=$fps audio=${audio:-none} first-frame-exit=$first first-frame-errors=$(wc -l < "$L.first")"
  if [ "$codec" = "$2" ] && [ "$res" = "$3" ] && [ "$fpsok" = 1 ] && [ "$audio" = aac ] \
     && [ $first = 0 ] && [ ! -s "$L.first" ]; then pass "A1 $msg"; else fail "A1 $msg"; cat "$L.err"; fi
}
if want A1; then a1 main $MAIN_CODEC $MAIN_RES; a1 sub $SUB_CODEC $SUB_RES; fi

# ---- A2 / A2b: capture with -c copy, then decode the file with -v error ---------------------
capture() { # tag stream secs transport-args...; runs in the background, sets CAP
  tag=$1; s=$2; secs=$3; shift 3
  timeout $((secs+40)) "$FFMPEG" -nostdin -loglevel debug "$@" -i "$(url "$s")" -t "$secs" -map 0 -c copy \
    -y "$W/$tag.mkv" > "$W/$tag.capture.log" 2>&1 &
  CAP=$!
}
check_capture() { # tag secs test-name
  tag=$1; secs=$2
  timeout $((secs+60)) "$FFMPEG" -nostdin -v error -i "$W/$tag.mkv" -f null - > "$W/$tag.decode.log" 2>&1
  dec=$?
  errs=$(wc -l < "$W/$tag.decode.log")
  bad=$(count 'RTP: missed|PPS id out of range' "$W/$tag.capture.log")
  frames=$("$FFPROBE" -v error -select_streams v:0 -count_packets -show_entries stream=nb_read_packets \
    -of csv=p=0 "$W/$tag.mkv" 2>/dev/null)
  afr=$("$FFPROBE" -v error -select_streams a:0 -count_packets -show_entries stream=nb_read_packets \
    -of csv=p=0 "$W/$tag.mkv" 2>/dev/null)
  minf=$((secs * FPS * 95 / 100))
  msg="$tag: ${secs}s video-frames=${frames:-0} (min $minf) audio-frames=${afr:-0} decode-errors=$errs missed/PPS=$bad"
  if [ $dec = 0 ] && [ "$errs" = 0 ] && [ "$bad" = 0 ] && [ "${frames:-0}" -ge $minf ] && [ "${afr:-0}" -gt 0 ]; then
    pass "$3 $msg"
  else
    fail "$3 $msg"; head -5 "$W/$tag.decode.log"; grep -E 'RTP: missed|PPS id out of range' "$W/$tag.capture.log" | head -5
  fi
}
if want A2; then
  capture a2.main main "$A2_SECS" -rtsp_transport tcp; C1=$CAP
  capture a2.sub sub "$A2_SECS" -rtsp_transport tcp; C2=$CAP
  wait $C1; wait $C2
  check_capture a2.main "$A2_SECS" A2; check_capture a2.sub "$A2_SECS" A2
fi
if want A2b; then
  capture a2b.main main "$A2B_SECS"; wait $CAP
  # ffmpeg tries UDP first: SETUP must get 461. The server only does interleaved TCP, so
  # media in the capture means the retry was TCP.
  if grep -q 'SETUP failed: 461' "$W/a2b.main.capture.log"; then
    echo "     A2b main: the UDP SETUP got 461; ffmpeg retried over TCP"
    check_capture a2b.main "$A2B_SECS" A2b
  else
    fail "A2b main: no 461 for the UDP SETUP in the ffmpeg log"
  fi
fi

# ---- go2rtc (A4, A4b, A10) -------------------------------------------------------------------
G2R=""
go2rtc_start() {
  [ -n "$G2R" ] && return
  {
    echo "log: {level: ${G2R_LOG:-debug}}"
    echo "api: {listen: \"127.0.0.1:$G2R_API\"}"
    echo "rtsp: {listen: \"127.0.0.1:$G2R_RTSP\"}"
    echo "webrtc: {listen: \"\"}"
    echo "srtp: {listen: \"\"}"
    echo "rtmp: {listen: \"\"}"
    echo "ffmpeg: {bin: $(command -v "$FFMPEG")}"
    echo "streams:"
    echo "  main: rtsp://127.0.0.1:$PORT/argus/main"
    echo "  sub: rtsp://127.0.0.1:$PORT/argus/sub"
    for m in $A4B_MODES; do echo "  rp_${m}_main:"; echo "  rp_${m}_sub:"; done
    echo "  a10_none: rtsp://127.0.0.1:$((PORT+20))/argus/main"
    echo "  a10_wrong: rtsp://alice:wrong@127.0.0.1:$((PORT+20))/argus/main"
    echo "  a10_basic: rtsp://alice:alicepw@127.0.0.1:$((PORT+21))/argus/main"
    echo "  a10_digest: rtsp://alice:alicepw@127.0.0.1:$((PORT+22))/argus/main"
    echo "  a10_notpermitted: rtsp://bob:bobpw@127.0.0.1:$((PORT+20))/argus/main"
    echo "  a10_anyone_valid: rtsp://bob:bobpw@127.0.0.1:$((PORT+23))/argus/main"
    echo "  a10_anyone_none: rtsp://127.0.0.1:$((PORT+23))/argus/main"
    for l in $A11B_LATENCIES; do echo "  a11b_$l: rtsp://127.0.0.1:$((PORT+24+$(a11b_idx "$l")))/argus/main"; done
  } > "$W/go2rtc.yaml"
  go2rtc -config "$W/go2rtc.yaml" > "$W/go2rtc.log" 2>&1 &
  G2R=$!; PIDS="$PIDS $G2R"
  i=0; until curl -s -o /dev/null "http://127.0.0.1:$G2R_API/api" || [ $i -gt 100 ]; do sleep 0.1; i=$((i+1)); done
  curl -s -o /dev/null "http://127.0.0.1:$G2R_API/api" || { echo "go2rtc did not start:"; tail -20 "$W/go2rtc.log"; exit 2; }
}
frame() { # src out -> 0 if a JPEG came back; sets JSIZE, JCODE
  JSIZE=0; JCODE=000
  for _ in 1 2 3; do
    JCODE=$(curl -s -m 30 -o "$2" -w '%{http_code}' "http://127.0.0.1:$G2R_API/api/frame.jpeg?src=$1")
    if [ "$(head -c 2 "$2" 2>/dev/null | od -An -tx1 | tr -d ' ')" = ffd8 ]; then
      JSIZE=$(wc -c < "$2"); return 0
    fi
    sleep 2
  done
  return 1
}
if want A4; then
  go2rtc_start
  for s in main sub; do
    if frame "$s" "$W/a4.$s.jpg"; then pass "A4 $s: go2rtc frame.jpeg is a JPEG ($JSIZE bytes)"
    else fail "A4 $s: go2rtc frame.jpeg: HTTP $JCODE, $(head -c 200 "$W/a4.$s.jpg")"; fi
  done
fi
if want A4b; then
  go2rtc_start
  p=$((PORT+10))
  for m in $A4B_MODES; do
    serve $p "a4b.$m" --parameter-sets "$m"; S4=$SRV
    for s in main sub; do
      timeout 120 "$FFMPEG" -nostdin -v warning -rtsp_transport tcp -i "rtsp://127.0.0.1:$p/argus/$s" -map 0 -c copy \
        -f rtsp -rtsp_transport tcp "rtsp://127.0.0.1:$G2R_RTSP/rp_${m}_$s" > "$W/a4b.$m.$s.ffmpeg.log" 2>&1 &
      RP=$!; PIDS="$PIDS $RP"
      sleep 4
      case " $A4B_GATE " in *" $m "*) ok=pass; ko=fail;; *) ok=info; ko=info;; esac
      if frame "rp_${m}_$s" "$W/a4b.$m.$s.jpg"; then
        $ok "A4b parameter_sets=$m $s: neolink -> ffmpeg -c copy -f rtsp -> go2rtc frame.jpeg is a JPEG ($JSIZE bytes)"
      else
        $ko "A4b parameter_sets=$m $s: FAILED, HTTP $JCODE, $(head -c 200 "$W/a4b.$m.$s.jpg")"; tail -3 "$W/a4b.$m.$s.ffmpeg.log"
      fi
      stop $RP
    done
    stop_server $S4 "a4b.$m"; p=$((p+1))
  done
fi

# ---- A5: VLC over TCP, default caching -------------------------------------------------------
vlc_rc() { # command -> output (VLC's rc interface)
  python3 -I -c '
import socket, sys, time
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=5)
s.sendall((sys.argv[2] + "\n").encode()); time.sleep(1.5); s.settimeout(1); out = b""
try:
    while True:
        b = s.recv(65536)
        if not b: break
        out += b
except Exception: pass
sys.stdout.write(out.decode(errors="replace"))' "$VLC_RC" "$1"
}
stat() { sed -n "s/.*$1 *: *\([0-9]*\).*/\1/p" "$2" | tail -1; }
if want A5; then
  # Not --run-time: VLC 3 compares it with the input's media time, which jumps when live555
  # first syncs with an RTCP SR, so on a long-running server VLC stops seconds in. The rc
  # interface reads the stats and quits instead; timeout is the safety net.
  timeout $((A5_SECS+30)) runuser -u vlc -- cvlc -I rc --rc-host "127.0.0.1:$VLC_RC" --rtsp-tcp --play-and-exit \
    --vout=dummy --aout=dummy --no-video-title-show --stats -vv \
    "$(url main)" > "$W/a5.vlc.log" 2>&1 < /dev/null &
  VLC=$!; PIDS="$PIDS $VLC"
  sleep "$A5_SECS"
  vlc_rc stats > "$W/a5.stats"
  vlc_rc quit > /dev/null; sleep 1; stop $VLC
  dec=$(stat 'video decoded' "$W/a5.stats"); adec=$(stat 'audio decoded' "$W/a5.stats")
  shown=$(stat 'displayed' "$W/a5.stats"); lost=$(stat 'lost' "$W/a5.stats")
  late=$(count 'picture is too late' "$W/a5.vlc.log")
  minf=$(( (A5_SECS - 5) * FPS * 9 / 10 ))
  msg="main ${A5_SECS}s: decoded=${dec:-0} displayed=${shown:-0} (min $minf) lost=${lost:-?} audio-decoded=${adec:-0} too-late=$late"
  if [ "${shown:-0}" -ge $minf ] && [ "${adec:-0}" -gt 0 ] && [ $((late * 60)) -lt "$A5_SECS" ]; then pass "A5 $msg"
  else fail "A5 $msg"; grep -i -E 'error|too late' "$W/a5.vlc.log" | head -5; fi
fi

# ---- A11b: congestion episodes (forward wire-clock rebases), ffmpeg, go2rtc and VLC -------------
# The looped congestion fixture (an episode ~58 s into each 120 s lap) at each pacing
# latency in A11B_LATENCIES, all at once, for A11B_SECS (2 episodes need 230 s: the second
# catch-up skip is at ~218 s). The server logs each rebase over 1 s at info.
a11b_vlc_poll() { # rc-port secs out: "secs-since-start displayed" once a second, then quit
  python3 -I -c '
import re, socket, sys, time
port, secs, out = int(sys.argv[1]), float(sys.argv[2]), sys.argv[3]
t0 = time.monotonic(); s = None
with open(out, "w") as f:
    while time.monotonic() - t0 < secs:
        t = time.monotonic()
        try:
            if s is None:
                s = socket.create_connection(("127.0.0.1", port), timeout=2); s.settimeout(0.3)
            s.sendall(b"stats\n"); time.sleep(0.3); buf = b""
            try:
                while True:
                    b = s.recv(65536)
                    if not b: break
                    buf += b
            except socket.timeout: pass
            m = re.findall(rb"frames displayed\s*:\s*(\d+)", buf)
            if m: f.write("%.1f %s\n" % (t - t0, m[-1].decode())); f.flush()
        except OSError: s = None
        time.sleep(max(0.0, 1 - (time.monotonic() - t)))
    s and s.close()' "$@"
}
a11b_run() { # latency-ms: one server and its clients; writes $W/a11b.$1.*
  l=$1; i=$(a11b_idx "$l"); p=$((PORT+24+i)); rc=$((PORT+26+i)); L=$W/a11b.$l
  "$BIN/xtask" rtsp-serve --bind 127.0.0.1:$p --main "$F/congestion_h265.bcm" --loop --latency-ms "$l" \
    --ready-file "$L.ready" --stats "$L.stats.json" > "$L.server.log" 2>&1 &
  srv=$!
  wait_for "$L.ready" || { echo "a11b $l: the server did not start" > "$L.err"; kill $srv; return; }
  # Recorders: -c copy, stopped by SIGINT at the end (exit 124 from timeout: it ran to the end)
  timeout -s INT "$A11B_SECS" "$FFMPEG" -nostdin -v error -rtsp_transport tcp -i "rtsp://127.0.0.1:$p/argus/main" \
    -map 0 -c copy -y "$L.ffmpeg.mkv" > "$L.ffmpeg.log" 2>&1 &
  f1=$!
  timeout -s INT "$A11B_SECS" "$FFMPEG" -nostdin -v error -rtsp_transport tcp -i "rtsp://127.0.0.1:$G2R_RTSP/a11b_$l" \
    -map 0 -c copy -y "$L.go2rtc.mkv" > "$L.go2rtc.log" 2>&1 &
  f2=$!
  timeout $((A11B_SECS+30)) runuser -u vlc -- cvlc -I rc --rc-host "127.0.0.1:$rc" --rtsp-tcp \
    --vout=dummy --aout=dummy --no-video-title-show --stats -vv "rtsp://127.0.0.1:$p/argus/main" \
    > "$L.vlc.log" 2>&1 < /dev/null &
  v=$!
  a11b_vlc_poll "$rc" "$A11B_SECS" "$L.vlc.stats"
  e1=0; wait $f1 || e1=$?; e2=0; wait $f2 || e2=$?
  sleep 2; vx=0; kill -0 $v 2>/dev/null || vx=1; kill -TERM $v 2>/dev/null; wait $v 2>/dev/null
  echo "$e1 $e2 $vx" > "$L.exits"
  if kill -0 $srv 2>/dev/null; then stop $srv; else echo "a11b $l: the server exited" > "$L.err"; fi
}
a11b_rec() { # latency client exit: the recording ran to the end, decodes cleanly, timeline
  # tracks wall time, A/V stays in sync across the rebases
  l=$1; c=$2; RL=$W/a11b.$l.$c
  dec=1; errs="?"; span=0
  if [ -s "$RL.mkv" ]; then
    timeout 120 "$FFMPEG" -nostdin -v error -i "$RL.mkv" -f null - > "$RL.decode.log" 2>&1; dec=$?
    errs=$(wc -l < "$RL.decode.log")
    span=$("$FFPROBE" -v error -select_streams v:0 -show_entries packet=pts_time -of csv=p=0 "$RL.mkv" \
      | awk -F, '$1 != "" && $1 != "N/A" {if (n++ == 0) lo = $1; if ($1 > hi) hi = $1} END {printf "%.1f", hi - lo}')
    # Audio pts must never go back
    aback=$("$FFPROBE" -v error -select_streams a:0 -show_entries packet=pts_time -of csv=p=0 "$RL.mkv" \
      | awk -F, '$1 != "" && $1 != "N/A" {if (n++ && $1 < p) b++; p = $1} END {print b + 0}')
    "$BIN/xtask" avsync --min-pairs 1 "$RL.mkv" > "$RL.avsync" 2>&1; av=$?
  else aback="?"; av=1; echo "no recording" > "$RL.avsync"; fi
  pairs=$(sed -n 's/^pairs \([0-9]*\).*/\1/p' "$RL.avsync"); worst=$(sed -n 's/.*worst |\([0-9.]*\)|.*/\1/p' "$RL.avsync")
  minpairs=$(awk -v s="$span" 'BEGIN {printf "%d", s / 2 * 0.75}')
  # Wire time follows wall time: the recording spans the run less the start-up (about 4 s);
  # a ratchet ahead (the bug PR #6 fixed: about +4 s per bridged episode) makes it longer
  drift=$(awk -v s="$span" -v w="$A11B_SECS" 'BEGIN {printf "%.1f", s - w}')
  driftok=$(awk -v d="$drift" 'BEGIN {print (d >= -10 && d <= 2) ? 1 : 0}')
  msg="latency ${l}ms $c: exit=$3 (124: ran to the end) decode-exit=$dec decode-errors=$errs span=${span}s (run ${A11B_SECS}s, drift ${drift}s, want -10..+2) audio-pts-back=$aback A/V pairs=${pairs:-0} (min $minpairs) worst=${worst:-?}ms (max 150)"
  if [ "$3" = 124 ] && [ $dec = 0 ] && [ "$errs" = 0 ] && [ "$driftok" = 1 ] && [ "$aback" = 0 ] \
     && [ $av = 0 ] && [ "${pairs:-0}" -ge "$minpairs" ]; then pass "A11b $msg"
  else fail "A11b $msg"; head -3 "$RL.decode.log" 2>/dev/null; tail -3 "$RL.avsync"; tail -3 "$W/a11b.$l.$c.log"; fi
}
if want A11b; then
  if [ "${SERVER:-rtsp-serve}" = neolink ]; then
    info "A11b: rtsp-serve only (it needs --latency-ms and the congestion fixture)"
  else
    go2rtc_start
    A11B_PIDS=""
    for l in $A11B_LATENCIES; do a11b_run "$l" & A11B_PIDS="$A11B_PIDS $!"; done
    PIDS="$PIDS $A11B_PIDS"
    for p in $A11B_PIDS; do wait "$p"; done
    for l in $A11B_LATENCIES; do
      L=$W/a11b.$l
      if [ -s "$L.err" ]; then fail "A11b latency ${l}ms: $(cat "$L.err")"; tail -5 "$L.server.log"; continue; fi
      # The episodes happened: logged forward rebases over 1 s, and the guard never engaged
      reb=$(count 'INFO .*wire clock rebased' "$L.server.log")
      guard=$(sed -n 's/.*"guard_engaged":\([0-9]*\).*/\1/p' "$L.stats.json")
      msg="latency ${l}ms: $reb logged rebases > 1 s (min 2), guard_engaged=${guard:-?}"
      if [ "$reb" -ge 2 ] && [ "${guard:-1}" = 0 ]; then pass "A11b $msg"; else fail "A11b $msg"; fi
      read -r e1 e2 vx < "$L.exits"
      a11b_rec "$l" ffmpeg "$e1"
      a11b_rec "$l" go2rtc "$e2"
      # VLC: still playing at the end, 85 % of the frames shown, no display freeze longer
      # than A11B_VLC_STALL. Measured: at most 14 s, at an underrun, not a rebase: frames
      # come late after an arrival gap, VLC raises its pts_delay (to 2.8 s by the second
      # episode) and re-buffers; after a 13 s forward rebase it recovers in 5-8 s. A client
      # that froze for the length of each step would fail both bounds
      st=$(awk -v w="$A11B_SECS" 'NR == 1 {lt = $1; ld = $2} $2 > ld {if ($1 - lt > m) m = $1 - lt; lt = $1}
        {ld = $2; last = $1} END {if (last - lt > m) m = last - lt; printf "%.0f %d", m, ld}' "$L.vlc.stats")
      stall=${st% *}; shown=${st#* }
      minf=$((A11B_SECS * FPS * 85 / 100))
      msg="latency ${l}ms vlc: playing-at-end=$((1 - vx)) displayed=${shown:-0} (min $minf) longest-freeze=${stall:-?}s (max $A11B_VLC_STALL) too-late=$(count 'picture is too late' "$L.vlc.log")"
      if [ "$vx" = 0 ] && [ "${shown:-0}" -ge $minf ] && [ "${stall:-99}" -le "$A11B_VLC_STALL" ]; then pass "A11b $msg"
      else fail "A11b $msg"; grep -E 'PCR|deadlock|buffering done' "$L.vlc.log" | head -8; fi
    done
  fi
fi

# ---- A10: the auth matrix, per client --------------------------------------------------------
# Rows: name port creds expected (401, 404 or play)
A10_ROWS="none:20::401 wrong:20:alice:wrong@:401 basic:21:alice:alicepw@:play digest:22:alice:alicepw@:play
notpermitted:20:bob:bobpw@:404 anyone_valid:23:bob:bobpw@:play anyone_none:23::401"
row() { # "name:port:creds:expected" -> R_NAME R_PORT R_CREDS R_EXP
  R_NAME=${1%%:*}; r=${1#*:}; R_PORT=$((PORT + ${r%%:*})); r=${r#*:}; R_EXP=${r##*:}; R_CREDS=${r%:*}
}
a10_result() { # client expected got detail
  if [ "$3" = "$2" ]; then pass "A10 $1 $R_NAME: $3 ${4:-}"; else fail "A10 $1 $R_NAME: expected $2, got $3 ${4:-}"; fi
}
if want A10; then
  serve $((PORT+20)) a10.both --user alice:alicepw --user bob:bobpw --permit alice; A1S=$SRV
  serve $((PORT+21)) a10.basic --user alice:alicepw --user bob:bobpw --permit alice --auth basic; A2S=$SRV
  serve $((PORT+22)) a10.digest --user alice:alicepw --user bob:bobpw --permit alice --auth digest; A3S=$SRV
  serve $((PORT+23)) a10.anyone --user alice:alicepw --user bob:bobpw --permit anyone; A4S=$SRV
  go2rtc_start
  for r in $A10_ROWS; do
    row "$r"; U="rtsp://${R_CREDS}127.0.0.1:$R_PORT/argus/main"; L=$W/a10.$R_NAME
    # ffmpeg
    timeout 30 "$FFMPEG" -nostdin -v error -rtsp_transport tcp -i "$U" -map 0:v -t 3 -f null - > "$L.ffmpeg.log" 2>&1
    rc=$?
    if grep -q '401 Unauthorized' "$L.ffmpeg.log"; then got=401
    elif grep -q '404 Not Found' "$L.ffmpeg.log"; then got=404
    elif [ $rc = 0 ] && ! grep -q 'Server returned' "$L.ffmpeg.log"; then got=play
    else got="exit $rc: $(head -c 120 "$L.ffmpeg.log")"; fi
    a10_result ffmpeg "$R_EXP" "$got"
    # go2rtc: a JPEG when it plays; otherwise its log says why. It names auth failures
    # ("user/pass not provided", "wrong user/pass") but not the status of other refusals
    # ("wrong response on DESCRIBE"), which is counted as the 404 here.
    before=$(wc -l < "$W/go2rtc.log")
    if frame "a10_$R_NAME" "$L.jpg"; then got=play; d="(JPEG $JSIZE bytes)"
    else
      tail -n +$((before+1)) "$W/go2rtc.log" > "$L.go2rtc.log"
      d="(go2rtc: $(grep -o 'error="[^"]*"' "$L.go2rtc.log" | tail -1))"
      if grep -q 'user/pass not provided\|wrong user/pass' "$L.go2rtc.log"; then got=401
      elif grep -q 'wrong response on DESCRIBE' "$L.go2rtc.log"; then got=404
      else got=other; fi
    fi
    a10_result go2rtc "$R_EXP" "$got" "$d"
    # VLC
    timeout 25 runuser -u vlc -- cvlc -I dummy --rtsp-tcp --run-time=4 --play-and-exit --vout=dummy --aout=dummy -vv \
      "$U" > "$L.vlc.log" 2>&1 < /dev/null
    VL=$L.vlc.log
    # live555 retries a 401 with the credentials: what counts is the final answer
    last=$(grep -o '^RTSP/1.0 [0-9]*' "$VL" | tail -1 | cut -d' ' -f2)
    if grep -q 'Sending request: PLAY' "$VL" && [ "$last" = 200 ] \
       && grep -q 'using video decoder module' "$VL"; then got=play
    else got=${last:-none}; fi
    a10_result vlc "$R_EXP" "$got"
  done
  stop_server $A1S a10.both; stop_server $A2S a10.basic; stop_server $A3S a10.digest; stop_server $A4S a10.anyone
fi

stop_server $MAIN_SRV main
if [ $FAILS = 0 ]; then echo "ALL PASSED"; else echo "$FAILS FAILED"; fi
exit $FAILS
