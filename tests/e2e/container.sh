#!/usr/bin/env bash
# N9 / PLAN 1.9: the image stops in < 5 s with exit code 0 and logs the camera out.
# Runs on the host with podman and a built xtask (cargo build --release -p xtask). Needs an
# image built from ./Dockerfile (IMAGE).
#   IMAGE=neolink-test:ci bash tests/e2e/container.sh
# (tests/e2e/container-entry.sh covers this and much more, for both images.)
set -euo pipefail
. "$(dirname "$0")/lib.sh"
IMAGE=${IMAGE:?the image to test}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
XTASK=${XTASK:-${CARGO_TARGET_DIR:-$ROOT/target}/release/xtask}
W=$(mktemp -d)
T="neolink-n9-$$"
track_ctr "$T-neolink"
[ -x "$XTASK" ] || die "no xtask at $XTASK (cargo build --release -p xtask)"
"$XTASK" fakecam --addr-file "$W/addr" --stats "$W/stats.json" > "$W/fakecam.log" 2>&1 &
FAKE=$!; track $FAKE
wait_until 20 $FAKE "fake camera address" test -s "$W/addr"
printf 'bind = "127.0.0.1"\nbind_port = 38554\n[[cameras]]\nname = "fake"\nusername = "admin"\npassword = "fakepass"\naddress = "%s"\nidle_disconnect = false\n' "$(cat "$W/addr")" > "$W/neolink.toml"
podman run -d --name "$T-neolink" --network host -v "$W/neolink.toml":/etc/neolink.toml:ro,z "$IMAGE" >/dev/null
logged_in() { grep -q '"logins":1' "$W/stats.json" 2>/dev/null; }
wait_until 20 "" "camera login from the container" logged_in
START=$(date +%s%N)
timeout 30 podman stop -t 10 "$T-neolink" >/dev/null || die "podman stop failed"
MS=$(( ($(date +%s%N) - START) / 1000000 ))
CODE=$(podman inspect "$T-neolink" --format '{{.State.ExitCode}}')
sleep 0.5
LOGOUTS=$(sed -n 's/.*"logouts":\([0-9]*\).*/\1/p' "$W/stats.json")
echo "stop: ${MS} ms, exit code ${CODE}, logouts ${LOGOUTS:-none}"
if [ "$MS" -lt 5000 ] && [ "$CODE" = 0 ] && [ "${LOGOUTS:-}" = 1 ]; then echo PASS; exit 0; fi
echo FAIL; exit 1
