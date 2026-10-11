#!/bin/sh
# Gate 3 client acceptance (SPEC §7: A1, A2, A2b, A4, A4b, A5, A10; PLAN 3.9/3.11) against
# the native server (xtask rtsp-serve) on the synthetic fixtures, with real clients:
# ffprobe/ffmpeg, go2rtc and VLC (cvlc).
# Runs in the neolink-clients image (tests/e2e/clients/Containerfile) with --network host,
# the pinned static ffmpeg on PATH (/ffmpeg/bin) and a built xtask:
#   cargo build --release -p xtask && sh tests/e2e/clients.sh
# E2E_WORK must be a fresh directory (nothing in it is deleted or reused).
# Env: TESTS (default "A1 A2 A2b A4 A4b A5 A10"), A2_SECS (120), A2B_SECS (20),
#      A5_SECS (60; the full gate is 300), PSETS (the --parameter-sets mode for the main
#      server, default the server's own), A4B_MODES ("strip passthrough inject"),
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
  } > "$W/go2rtc.yaml"
  go2rtc -config "$W/go2rtc.yaml" > "$W/go2rtc.log" 2>&1 &
  G2R=$!; PIDS="$PIDS $G2R"
  i=0; until curl -s -o /dev/null "http://127.0.0.1:$G2R_API/api" || [ $i -gt 100 ]; do sleep 0.1; i=$((i+1)); done
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
    stop $S4; p=$((p+1))
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
  stop $A1S; stop $A2S; stop $A3S; stop $A4S
fi

stop $MAIN_SRV
[ $FAILS = 0 ] && echo "ALL PASSED" || echo "$FAILS FAILED"
exit $FAILS
