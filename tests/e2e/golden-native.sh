#!/usr/bin/env bash
# A19: the native server (xtask rtsp-serve on the fixtures) against the GStreamer golden
# transcripts, structurally, for every alias and auth case. The SPEC §5 deltas are
# expected: each diff line must match a pattern in crates/rtsp/tests/golden/native-deltas.txt.
# Runs in the dev container after `cargo build --release -p xtask`; needs ffmpeg on PATH.
# E2E_WORK must be a fresh directory (nothing in it is deleted or reused).
# Exit status: the number of failed cases; 1 when the run itself broke (a server that
# did not start or died, a recording or a diff that could not be made).
set -euo pipefail
. "$(dirname "$0")/lib.sh"
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
W=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$W/raw"
GOLD=$ROOT/crates/rtsp/tests/golden/gst
DELTAS=$ROOT/crates/rtsp/tests/golden/native-deltas.txt
F=$ROOT/fixtures/synth
PORT=${PORT:-48654}
N=0
FAKE=""
SRV=""
need "$DELTAS" "expected deltas"
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
  FAKE=$!; track $FAKE
  wait_until 10 $FAKE "fake camera address" test -s "$W/fake.$N"
  {
    printf 'bind = "127.0.0.1"\nbind_port = %s\nsnapshot_port = 0\nrtsp_backend = "native"\n' "$PORT"
    printf '[[cameras]]\nname = "argus"\nusername = "admin"\npassword = "fakepass"\naddress = "%s"\n' "$(cat "$W/fake.$N")"
    if [ -n "$permit" ]; then printf 'permitted_users = [%s]\n' "${permit%, }"; fi
    printf '%s\n' "$users"
  } > "$W/neolink.$N.toml"
  NEOLINK_STATE_DIR="$W/state.$N" RUST_LOG=info "$BIN/neolink" rtsp --config "$W/neolink.$N.toml" \
    > "$W/server.$N.log" 2>&1 &
  SRV=$!; track $SRV
  wait_until 30 $SRV "neolink start (see $W/server.$N.log)" grep -q "Starting RTSP Server (native)" "$W/server.$N.log"
}
stop_server() {
  stop_pid "$SRV" server
  if [ -n "$FAKE" ]; then stop_pid "$FAKE" "fake camera"; FAKE=""; fi
}
start() { # extra args
  if [ "${SERVER:-rtsp-serve}" = neolink ]; then start_neolink "$@"; return; fi
  N=$((N+1))
  "$BIN/xtask" rtsp-serve --bind 127.0.0.1:"$PORT" --main "$F/plain_h265.bcm" --sub "$F/plain_h264.bcm" \
    --loop --ready-file "$W/ready.$N" "$@" > "$W/server.$N.log" 2>&1 &
  SRV=$!; track $SRV
  wait_until 10 $SRV "rtsp-serve start (see $W/server.$N.log)" test -e "$W/ready.$N"
}
rec() { # name path transport creds
  alive "$SRV" server
  if [ -n "$FAKE" ]; then alive "$FAKE" "fake camera"; fi
  N=$((N+1))
  "$BIN/xtask" golden record --listen 127.0.0.1:$((PORT+1)) --upstream 127.0.0.1:"$PORT" \
    --out "$W/raw/$1.txt" --ready-file "$W/ready.$N" 2> "$W/$1.record.log" &
  local px=$!; track $px
  wait_until 10 $px "recording proxy for $1" test -e "$W/ready.$N"
  local t=(); [ "$3" = tcp ] && t=(-rtsp_transport tcp)
  # ffmpeg's own result is not the check (the auth and unknown-path cases must fail);
  # the transcript is
  timeout 30 ffmpeg -v error -nostdin "${t[@]}" -i "rtsp://${4:-}127.0.0.1:$((PORT+1))$2" -t 4 -f null - \
    > "$W/$1.ffmpeg.log" 2>&1 || true
  local rc=0
  wait_pid $px 15 "recording proxy for $1" || rc=$?
  [ $rc = 0 ] || die "recording proxy for $1 exited $rc (see $W/$1.record.log)"
  need "$W/raw/$1.txt" "transcript of $1"
}
FAILS=0
# drop: grep -v -f PATTERNS IN > OUT, where "no line left" is fine and an error is not
drop() { grep -v -E -f "$1" "$2" > "$3" || [ $? = 1 ] || die "grep -f $1 failed"; }
diff_gold() {
  # 461 then a TCP retry is the expected UDP behaviour (A2b): compare what follows.
  # golden diff: 0 same, 1 differs (the lines are the diff), anything else broke
  local rc=0
  "$BIN/xtask" golden diff --drop-status 461 "$GOLD/$1.txt" "$W/raw/$1.txt" > "$W/$1.diff" 2> "$W/$1.diff.err" || rc=$?
  case $rc in
    0) ;;
    1) need "$W/$1.diff" "golden diff output of $1 (it said they differ)";;
    *) echo "FAIL $1: golden diff exited $rc"; sed 's/^/    /' "$W/$1.diff.err"; FAILS=$((FAILS+1)); return;;
  esac
  # Lines not explained by an expected delta, global or for this case
  CASE=$ROOT/crates/rtsp/tests/golden/native-deltas.d/$1.txt
  [ -f "$CASE" ] || CASE=$DELTAS
  drop "$DELTAS" "$W/$1.diff" "$W/$1.d1"
  drop "$CASE" "$W/$1.d1" "$W/$1.unexpected"
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
  # shellcheck disable=SC2086
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
if [ $FAILS = 0 ]; then echo "ALL PASSED"; else echo "$FAILS FAILED"; fi
exit $FAILS
