#!/bin/sh
# Gate 1: the GStreamer backend after Phase 1 still matches the 2026-10-11 golden
# transcripts structurally. Known pre-existing delta: GStreamer answers PLAY with 503 to
# every second back-to-back client on the fake (open_sub_*, open_bare_tcp); the
# pre-Phase-1 binary does the same, so compare against BASELINE for regressions. Serves the fake camera (H.265 main, H.264 sub like the Argus)
# under the name "argus" so paths match, records with the proxy, diffs.
# Runs in the dev container after building neolink and xtask (release).
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
W=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$W/raw"
# Compare with the committed camera goldens, or with an earlier run of this script
# (BASELINE=<its E2E_WORK>), e.g. one made with the previous binary (NEOLINK_BIN=...)
GOLD=${BASELINE:+$BASELINE/raw}
GOLD=${GOLD:-$ROOT/crates/rtsp/tests/golden/gst}
"$BIN/xtask" fakecam --addr-file "$W/addr" --stats "$W/stats.json" \
  --main "$ROOT/fixtures/synth/plain_h265.bcm" --sub "$ROOT/fixtures/synth/plain_h264.bcm" 2>/dev/null &
FC=$!
i=0; until [ -s "$W/addr" ] || [ $i -gt 50 ]; do sleep 0.1; i=$((i+1)); done
write_config() { # users?
  printf 'bind = "127.0.0.1"\nbind_port = 48554\nsnapshot_port = 0\n' > "$W/n.toml"
  [ "$1" = users ] && printf '[[users]]\nname = "alice"\npass = "alicepw"\n[[users]]\nname = "bob"\npass = "bobpw"\n' >> "$W/n.toml"
  printf '[[cameras]]\nname = "argus"\nusername = "admin"\npassword = "fakepass"\naddress = "%s"\n' "$(cat "$W/addr")" >> "$W/n.toml"
  [ "$1" = users ] && printf 'permitted_users = ["alice"]\n' >> "$W/n.toml"
}
start() {
  RUST_LOG=info "${NEOLINK_BIN:-$BIN/neolink}" rtsp --config "$W/n.toml" > "$W/neolink_$1.log" 2>&1 &
  NL=$!
  i=0; until grep -q "argus: Available at.*sub" "$W/neolink_$1.log" || [ $i -gt 300 ]; do sleep 0.1; i=$((i+1)); done
}
FLAKES=0
rec() { # name path transport creds; retries the GStreamer PLAY 503 flake (see top)
  for attempt in 1 2 3; do
    rec_once "$@"
    grep -q "^< RTSP/1.0 503" "$W/raw/$1.txt" || return 0
    FLAKES=$((FLAKES+1)); sleep 3
  done
}
rec_once() {
  rm -f "$W/ready"
  "$BIN/xtask" golden record --listen 127.0.0.1:48555 --upstream 127.0.0.1:48554 --out "$W/raw/$1.txt" --ready-file "$W/ready" 2>/dev/null &
  PX=$!
  i=0; until [ -e "$W/ready" ] || [ $i -gt 100 ]; do sleep 0.1; i=$((i+1)); done
  T=""; [ "$3" = tcp ] && T="-rtsp_transport tcp"
  timeout 30 ffmpeg -v error -nostdin $T -i "rtsp://${4:-}127.0.0.1:48555$2" -t 4 -f null - > /dev/null 2>&1
  wait $PX
}
FAILS=0
diff_gold() {
  if "$BIN/xtask" golden diff "$GOLD/$1.txt" "$W/raw/$1.txt" > "$W/$1.diff"; then
    echo "PASS $1"
  else
    echo "FAIL $1:"; sed 's/^/    /' "$W/$1.diff"; FAILS=$((FAILS+1))
  fi
}
write_config none; start open
for c in "open_main_tcp /argus/main tcp" "open_sub_tcp /argus/sub tcp" "open_mainStream_tcp /argus/mainStream tcp" \
         "open_bare_tcp /argus tcp" "open_main_udp /argus/main udp" "open_sub_udp /argus/sub udp" "open_unknown_path /nope/main tcp"; do
  set -- $c; rec "$1" "$2" "$3"; diff_gold "$1"
done
kill -TERM $NL; wait $NL
write_config users; start auth
rec auth_none /argus/main tcp; diff_gold auth_none
rec auth_wrong_pass /argus/main tcp alice:wrong@; diff_gold auth_wrong_pass
rec auth_not_permitted /argus/main tcp bob:bobpw@; diff_gold auth_not_permitted
rec auth_ok /argus/main tcp alice:alicepw@; diff_gold auth_ok
kill -TERM $NL; wait $NL
kill -TERM $FC
echo "GStreamer PLAY 503 flakes retried: $FLAKES"
[ $FAILS = 0 ] && echo "ALL PASSED" || echo "$FAILS FAILED"
exit $FAILS
