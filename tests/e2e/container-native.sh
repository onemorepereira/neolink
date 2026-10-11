#!/bin/sh
# A13 / N9 / PLAN 6.3 for the native image (Dockerfile.native, FROM scratch):
# - the image starts and serves RTSP from a scripted camera (xtask fakecam);
# - with a client playing, `podman stop` takes < 5 s, exits 0 and the camera is logged out;
# - PUID/PGID drop privileges.
# Runs on the host with podman. Needs the native image (IMAGE) and the dev image with a
# built release xtask in the neolink-target volume, plus ffmpeg in the dev image's PATH
# (mounted at /ffmpeg as in the dev wrappers; FFMPEG_DIR on the host).
#   IMAGE=neolink:native FFMPEG_DIR=/path/to/ffmpeg sh tests/e2e/container-native.sh
set -u
IMAGE=${IMAGE:-neolink:native}
FFMPEG_DIR=${FFMPEG_DIR:?the static ffmpeg directory (with bin/ffmpeg)}
PORT=${PORT:-38654}
W=$(mktemp -d)
FAILS=0
podman run -d --rm --name fakecam-a13 --network host -v "$W":/w:z -v neolink-target:/target \
  -v "$(cd "$(dirname "$0")/../.." && pwd)/fixtures/synth":/fx:ro,z \
  neolink-dev /target/release/xtask fakecam --addr-file /w/addr --stats /w/stats.json \
  --main /fx/plain_h265.bcm --sub /fx/plain_h264.bcm >/dev/null
i=0; until [ -s "$W/addr" ] || [ $i -gt 100 ]; do sleep 0.1; i=$((i+1)); done
printf 'bind = "127.0.0.1"\nbind_port = %s\nsnapshot_port = 0\n[[cameras]]\nname = "fake"\nusername = "admin"\npassword = "fakepass"\naddress = "%s"\n' \
  $PORT "$(cat "$W/addr")" > "$W/neolink.toml"
chmod 0644 "$W/neolink.toml"
podman run -d --name neolink-a13 --network host -e PUID=1000 -e PGID=1000 \
  -v "$W/neolink.toml":/etc/neolink.toml:ro,z "$IMAGE" >/dev/null
i=0; until podman logs neolink-a13 2>&1 | grep -q "Starting RTSP Server (native)" || [ $i -gt 100 ]; do sleep 0.1; i=$((i+1)); done
if podman logs neolink-a13 2>&1 | grep -q "Running as uid 1000, gid 1000"; then echo "PASS PUID/PGID drop"
else echo "FAIL PUID/PGID drop"; FAILS=$((FAILS+1)); fi
# A13: a client plays from the FROM-scratch image
podman run -d --rm --name client-a13 --network host -v "$FFMPEG_DIR":/ffmpeg:ro,z -v "$W":/w:z neolink-dev \
  /ffmpeg/bin/ffmpeg -nostdin -v error -rtsp_transport tcp -i "rtsp://127.0.0.1:$PORT/fake/main" \
  -c copy -f null - >/dev/null
sleep 8
if podman ps --format '{{.Names}}' | grep -q client-a13; then echo "PASS A13 the image serves a playing client"
else echo "FAIL A13 the client ended early"; FAILS=$((FAILS+1)); fi
# N9: stop with the client playing
START=$(date +%s%N)
podman stop -t 10 neolink-a13 >/dev/null
MS=$(( ($(date +%s%N) - START) / 1000000 ))
CODE=$(podman inspect neolink-a13 --format '{{.State.ExitCode}}')
sleep 1
LOGOUTS=$(sed -n 's/.*"logouts":\([0-9]*\).*/\1/p' "$W/stats.json")
podman logs neolink-a13 > "$W/neolink.log" 2>&1
podman rm -f neolink-a13 >/dev/null
podman stop -t 2 client-a13 >/dev/null 2>&1
podman stop -t 2 fakecam-a13 >/dev/null 2>&1
msg="stop with a client playing: ${MS} ms, exit code ${CODE}, logouts ${LOGOUTS}"
if [ "$MS" -lt 5000 ] && [ "$CODE" = 0 ] && [ "$LOGOUTS" = 1 ]; then echo "PASS N9 $msg"
else echo "FAIL N9 $msg"; tail -5 "$W/neolink.log" | sed 's/^/    /'; FAILS=$((FAILS+1)); fi
[ $FAILS = 0 ] && echo "ALL PASSED" || echo "$FAILS FAILED"
exit $FAILS
