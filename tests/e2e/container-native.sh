#!/usr/bin/env bash
# A13 / N9 / PLAN 6.3 for the native image (Dockerfile.native, FROM scratch):
# - the image starts and serves RTSP from a scripted camera (xtask fakecam): a client
#   receives frames from it;
# - with that client playing, `podman stop` takes < 5 s, exits 0 and the camera is logged out;
# - PUID/PGID drop privileges.
# Runs on the host with podman, a built xtask (the fake camera runs on the host, as in
# container-entry.sh) and ffmpeg (FFMPEG, default the one on PATH):
#   cargo build --release -p xtask
#   podman build -t neolink:native -f Dockerfile.native .
#   IMAGE=neolink:native bash tests/e2e/container-native.sh
# Exit status: the number of failed checks; 1 when the run itself broke.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
IMAGE=${IMAGE:-neolink:native}
PORT=${PORT:-38654}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
XTASK=${XTASK:-${CARGO_TARGET_DIR:-$ROOT/target}/release/xtask}
FFMPEG=${FFMPEG:-ffmpeg}
W=$(mktemp -d)
FAILS=0
T="neolink-a13-$$"
track_ctr "$T-neolink"
[ -x "$XTASK" ] || die "no xtask at $XTASK (cargo build --release -p xtask)"
command -v "$FFMPEG" >/dev/null || die "no ffmpeg ($FFMPEG)"
podman image exists "$IMAGE" || die "no image $IMAGE"
"$XTASK" fakecam --addr-file "$W/addr" --stats "$W/stats.json" \
  --main "$ROOT/fixtures/synth/plain_h265.bcm" --sub "$ROOT/fixtures/synth/plain_h264.bcm" > "$W/fakecam.log" 2>&1 &
FAKE=$!; track $FAKE
running() { [ "$(podman inspect "$1" --format '{{.State.Running}}' 2>/dev/null)" = true ]; }
wait_until 20 $FAKE "fake camera address" test -s "$W/addr"
printf 'bind = "127.0.0.1"\nbind_port = %s\nsnapshot_port = 0\n[[cameras]]\nname = "fake"\nusername = "admin"\npassword = "fakepass"\naddress = "%s"\n' \
  "$PORT" "$(cat "$W/addr")" > "$W/neolink.toml"
chmod 0644 "$W/neolink.toml"
podman run -d --name "$T-neolink" --network host -e PUID=1000 -e PGID=1000 \
  -v "$W/neolink.toml":/etc/neolink.toml:ro,z "$IMAGE" >/dev/null
logs() { podman logs "$T-neolink" 2>&1 || true; }
# (grep on a variable: with pipefail, `podman logs | grep -q` fails when grep stops early)
has() { local l; l=$(logs); grep -q -- "$1" <<<"$l"; }
started() { has "Starting RTSP Server (native)"; }
wait_until 20 "" "neolink start in the container" started
running "$T-neolink" || die "the neolink container exited: $(logs | tail -5)"
if has "Running as uid 1000, gid 1000" && ! has "is not writable by PUID"; then
  echo "PASS PUID/PGID drop, state directory writable"
else echo "FAIL PUID/PGID drop or state directory"; { logs | grep -i "uid\|writable" || true; } | sed 's/^/    /'; FAILS=$((FAILS+1)); fi
# A13: a client plays from the FROM-scratch image. ffmpeg's progress report shows frames
# arriving; a client that merely has not exited yet is not enough.
"$FFMPEG" -nostdin -v error -rtsp_transport tcp -i "rtsp://127.0.0.1:$PORT/fake/main" \
  -c copy -f null -progress "$W/progress.txt" - > "$W/client.log" 2>&1 &
CLIENT=$!; track $CLIENT
sleep 8
frames=$({ sed -n 's/^frame=\([0-9]*\)$/\1/p' "$W/progress.txt" 2>/dev/null || true; } | tail -1)
if kill -0 $CLIENT 2>/dev/null && [ "${frames:-0}" -ge 25 ]; then echo "PASS A13 the image serves a playing client ($frames frames in 8 s)"
else echo "FAIL A13 the client is not playing (frames: ${frames:-none})"; sed 's/^/    /' "$W/client.log" | tail -5; FAILS=$((FAILS+1)); fi
# N9: stop with the client playing
running "$T-neolink" || die "the neolink container exited before the stop: $(logs | tail -5)"
START=$(date +%s%N)
timeout 30 podman stop -t 10 "$T-neolink" >/dev/null || die "podman stop failed"
MS=$(( ($(date +%s%N) - START) / 1000000 ))
CODE=$(podman inspect "$T-neolink" --format '{{.State.ExitCode}}')
sleep 1
need "$W/stats.json" "fake camera stats"
LOGOUTS=$(sed -n 's/.*"logouts":\([0-9]*\).*/\1/p' "$W/stats.json")
logs > "$W/neolink.log"
msg="stop with a client playing: ${MS} ms, exit code ${CODE}, logouts ${LOGOUTS:-none}"
if [ "$MS" -lt 5000 ] && [ "$CODE" = 0 ] && [ "${LOGOUTS:-}" = 1 ]; then echo "PASS N9 $msg"
else echo "FAIL N9 $msg"; tail -5 "$W/neolink.log" | sed 's/^/    /'; FAILS=$((FAILS+1)); fi
if [ $FAILS = 0 ]; then echo "ALL PASSED"; else echo "$FAILS FAILED"; fi
exit $FAILS
