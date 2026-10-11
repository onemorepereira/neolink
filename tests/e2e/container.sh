#!/bin/sh
# N9 / PLAN 1.9: the image stops in < 5 s with exit code 0 and logs the camera out.
# Runs on the host with podman. Needs an image built from this Dockerfile (IMAGE) and the
# dev image with a built xtask in the neolink-target volume.
#   IMAGE=neolink-test:latest sh tests/e2e/container.sh
set -u
IMAGE=${IMAGE:-neolink-test:latest}
W=$(mktemp -d)
podman run -d --rm --name fakecam-n9 --network host -v "$W":/n9:z -v neolink-target:/target \
  neolink-dev /target/release/xtask fakecam --addr-file /n9/addr --stats /n9/stats.json >/dev/null
i=0; until [ -s "$W/addr" ] || [ $i -gt 50 ]; do sleep 0.1; i=$((i+1)); done
printf 'bind = "127.0.0.1"\nbind_port = 38554\n[[cameras]]\nname = "fake"\nusername = "admin"\npassword = "fakepass"\naddress = "%s"\nidle_disconnect = false\n' "$(cat "$W/addr")" > "$W/neolink.toml"
podman run -d --name neolink-n9 --network host -v "$W/neolink.toml":/etc/neolink.toml:ro,z "$IMAGE" >/dev/null
i=0; until grep -q '"logins":1' "$W/stats.json" 2>/dev/null || [ $i -gt 200 ]; do sleep 0.1; i=$((i+1)); done
START=$(date +%s%N)
podman stop -t 10 neolink-n9 >/dev/null
MS=$(( ($(date +%s%N) - START) / 1000000 ))
CODE=$(podman inspect neolink-n9 --format '{{.State.ExitCode}}')
sleep 0.5
LOGOUTS=$(sed -n 's/.*"logouts":\([0-9]*\).*/\1/p' "$W/stats.json")
podman rm -f neolink-n9 >/dev/null
podman stop -t 2 fakecam-n9 >/dev/null
echo "stop: ${MS} ms, exit code ${CODE}, logouts ${LOGOUTS}"
[ "$MS" -lt 5000 ] && [ "$CODE" = 0 ] && [ "$LOGOUTS" = 1 ] && echo PASS && exit 0
echo FAIL; exit 1
