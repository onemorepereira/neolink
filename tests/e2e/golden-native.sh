#!/bin/sh
# A19: the native server (xtask rtsp-serve on the fixtures) against the GStreamer golden
# transcripts, structurally, for every alias and auth case. The SPEC §5 deltas are
# expected: each diff line must match a pattern in crates/rtsp/tests/golden/native-deltas.txt.
# Runs in the dev container after `cargo build --release -p xtask`; needs ffmpeg on PATH.
# E2E_WORK must be a fresh directory (nothing in it is deleted or reused).
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
W=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$W/raw"
GOLD=$ROOT/crates/rtsp/tests/golden/gst
DELTAS=$ROOT/crates/rtsp/tests/golden/native-deltas.txt
F=$ROOT/fixtures/synth
PORT=${PORT:-48654}
N=0
wait_for() { # file
  i=0; until [ -e "$1" ] || [ $i -gt 100 ]; do sleep 0.1; i=$((i+1)); done
}
# SERVER=neolink: `neolink rtsp` on the native backend against `xtask fakecam` (Gate 4);
# understands the --user and --permit options below
start_neolink() {
  N=$((N+1)); users=""; permit=""
  while [ $# -gt 0 ]; do
    case $1 in
      --user) users="$users
[[users]]
name = \"${2%%:*}\"
pass = \"${2#*:}\""; shift 2;;
      --permit) permit="$permit\"$2\", "; shift 2;;
      *) shift;;
    esac
  done
  "$BIN/xtask" fakecam --addr-file "$W/fake.$N" --stats "$W/fakestats.$N.json" \
    --main "$F/plain_h265.bcm" --sub "$F/plain_h264.bcm" > "$W/fake.$N.log" 2>&1 &
  FAKE=$!
  wait_for "$W/fake.$N"
  {
    printf 'bind = "127.0.0.1"\nbind_port = %s\nsnapshot_port = 0\nrtsp_backend = "native"\n' $PORT
    printf '[[cameras]]\nname = "argus"\nusername = "admin"\npassword = "fakepass"\naddress = "%s"\n' "$(cat "$W/fake.$N")"
    [ -n "$permit" ] && printf 'permitted_users = [%s]\n' "${permit%, }"
    printf '%s\n' "$users"
  } > "$W/neolink.$N.toml"
  NEOLINK_STATE_DIR="$W/state.$N" RUST_LOG=info "$BIN/neolink" rtsp --config "$W/neolink.$N.toml" \
    > "$W/server.$N.log" 2>&1 &
  SRV=$!
  i=0; until grep -q "Starting RTSP Server (native)" "$W/server.$N.log" || [ $i -gt 300 ]; do sleep 0.1; i=$((i+1)); done
}
stop_server() {
  kill -TERM $SRV; wait $SRV
  [ -n "${FAKE:-}" ] && { kill -TERM $FAKE; wait $FAKE; FAKE=""; }
}
start() { # extra args
  if [ "${SERVER:-rtsp-serve}" = neolink ]; then start_neolink "$@"; return; fi
  N=$((N+1))
  "$BIN/xtask" rtsp-serve --bind 127.0.0.1:$PORT --main "$F/plain_h265.bcm" --sub "$F/plain_h264.bcm" \
    --loop --ready-file "$W/ready.$N" "$@" > "$W/server.$N.log" 2>&1 &
  SRV=$!
  wait_for "$W/ready.$N"
}
rec() { # name path transport creds
  N=$((N+1))
  "$BIN/xtask" golden record --listen 127.0.0.1:$((PORT+1)) --upstream 127.0.0.1:$PORT \
    --out "$W/raw/$1.txt" --ready-file "$W/ready.$N" 2>/dev/null &
  PX=$!
  wait_for "$W/ready.$N"
  T=""; [ "$3" = tcp ] && T="-rtsp_transport tcp"
  timeout 30 ffmpeg -v error -nostdin $T -i "rtsp://${4:-}127.0.0.1:$((PORT+1))$2" -t 4 -f null - > "$W/$1.ffmpeg.log" 2>&1
  wait $PX
}
FAILS=0
diff_gold() {
  # 461 then a TCP retry is the expected UDP behaviour (A2b): compare what follows
  "$BIN/xtask" golden diff --drop-status 461 "$GOLD/$1.txt" "$W/raw/$1.txt" > "$W/$1.diff"
  # Lines not explained by an expected delta, global or for this case
  CASE=$ROOT/crates/rtsp/tests/golden/native-deltas.d/$1.txt
  [ -f "$CASE" ] || CASE=$DELTAS
  grep -v -E -f "$DELTAS" "$W/$1.diff" | grep -v -E -f "$CASE" > "$W/$1.unexpected"
  if [ -s "$W/$1.unexpected" ]; then
    echo "FAIL $1:"; sed 's/^/    /' "$W/$1.unexpected"; FAILS=$((FAILS+1))
  else
    echo "PASS $1 ($(wc -l < "$W/$1.diff") expected delta lines)"
  fi
}
start
for a in main Main mainStream MainStream Mainstream mainstream sub Sub subStream SubStream Substream substream \
         extern Extern externStream ExternStream Externstream externstream; do
  rec "open_${a}_tcp" "/argus/$a" tcp; diff_gold "open_${a}_tcp"
done
for c in "open_bare_tcp /argus tcp" "open_main_tcp_repeat /argus/main tcp" "open_main_udp /argus/main udp" \
         "open_sub_udp /argus/sub udp" "open_unknown_path /nope/main tcp"; do
  set -- $c; rec "$1" "$2" "$3"; diff_gold "$1"
done
stop_server
start --user alice:alicepw --user bob:bobpw --permit alice
rec auth_none /argus/main tcp; diff_gold auth_none
rec auth_wrong_pass /argus/main tcp alice:wrong@; diff_gold auth_wrong_pass
rec auth_unknown_user /argus/main tcp mallory:x@; diff_gold auth_unknown_user
rec auth_not_permitted /argus/main tcp bob:bobpw@; diff_gold auth_not_permitted
rec auth_ok /argus/main tcp alice:alicepw@; diff_gold auth_ok
stop_server
[ $FAILS = 0 ] && echo "ALL PASSED" || echo "$FAILS FAILED"
exit $FAILS
