#!/usr/bin/env bash
# The container entry path: every way people start the image, old and new.
#
# neolink is the image's entrypoint (exec form, PID 1). Its command line is the container's
# command, which may be neolink's arguments, neolink itself (compose files written for the
# old /entrypoint.sh) or any other program (`sh -c "..."`, which neolink execs). This runs
# the whole matrix against one image and fails on any mismatch:
# - command forms: default CMD, `neolink rtsp ...`, `/usr/local/bin/neolink mqtt-rtsp ...`,
#   `rtsp ...`, NEO_LINK_MODE with and without a command, `--config rtsp`, a config path
#   with spaces, `sh -c`, `bash -lc`, a script, `--entrypoint`, --help/--version;
# - signals: `podman stop` (SIGTERM) logs the camera out and exits 0 in < 5 s; SIGINT;
#   `--init`;
# - privileges and state: root, PUID/PGID, --user, --read-only, a state volume left
#   root-owned by an earlier root run, NEOLINK_STATE_DIR=/;
# - config errors: missing, unreadable, invalid.
# The GStreamer image (./Dockerfile) and the native one (./Dockerfile.native, no shell) are
# both covered; cases that need a shell in the image are swapped for their no-shell
# counterparts on the native image.
#
# Runs on the host with podman and a built xtask (the fake camera runs on the host):
#   cargo build --release -p xtask
#   podman build -t neolink:test -f Dockerfile .
#   IMAGE=neolink:test bash tests/e2e/container-entry.sh
#   podman build -t neolink:native -f Dockerfile.native .
#   IMAGE=neolink:native bash tests/e2e/container-entry.sh
# ONLY=<regex> runs the cases whose name matches; REQUIRE_LOGOUT=0 skips the logout check.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
IMAGE=${IMAGE:?the image to test, e.g. IMAGE=neolink:test}
XTASK=${XTASK:-$ROOT/target/release/xtask}
# A pinned helper image to look inside volumes (the native image has no shell)
HELPER=${HELPER:-docker.io/library/alpine:3.20}
# MQTT broker for the mqtt modes (without one, neolink's MQTT mode exits)
BROKER=${BROKER:-docker.io/library/eclipse-mosquitto:2.0.20}
# Below the ephemeral range (32768+), where outgoing connections could take a case's port
PORT_BASE=${PORT_BASE:-18800}
ONLY=${ONLY:-}
# 0 skips the "camera logged out on stop" check (the native backend does not log out a
# camera stopped while it is still learning its streams; that is not the entry path)
REQUIRE_LOGOUT=${REQUIRE_LOGOUT:-1}
TAG="neolink-entry-test-$$"
W=$(mktemp -d)
FAKECAM_PID=
FAILS=0
PASSES=0
CASE_N=0

cleanup() {
  local ids
  ids=$(podman ps -aq --filter "label=$TAG" 2>/dev/null || true)
  [ -n "$ids" ] && podman rm -f -t 0 $ids >/dev/null 2>&1 || true
  ids=$(podman volume ls -q --filter "label=$TAG" 2>/dev/null || true)
  [ -n "$ids" ] && podman volume rm -f $ids >/dev/null 2>&1 || true
  [ -n "$FAKECAM_PID" ] && kill "$FAKECAM_PID" 2>/dev/null || true
  # Files written by container users other than ours
  podman unshare rm -rf "$W" 2>/dev/null || rm -rf "$W"
}
trap cleanup EXIT

[ -x "$XTASK" ] || { echo "no xtask at $XTASK (cargo build --release -p xtask)"; exit 2; }
podman image exists "$IMAGE" || { echo "no image $IMAGE"; exit 2; }
podman image exists "$HELPER" || podman pull -q "$HELPER" >/dev/null
podman image exists "$BROKER" || podman pull -q "$BROKER" >/dev/null

if podman run --rm --label "$TAG" --entrypoint /bin/sh "$IMAGE" -c true >/dev/null 2>&1; then
  SHELL_IMAGE=1
else
  SHELL_IMAGE=0
fi
echo "image $IMAGE (shell: $SHELL_IMAGE)"

"$XTASK" fakecam --addr-file "$W/addr" --stats "$W/stats.json" >"$W/fakecam.log" 2>&1 &
FAKECAM_PID=$!
for _ in $(seq 100); do [ -s "$W/addr" ] && break; sleep 0.1; done
[ -s "$W/addr" ] || { echo "the fake camera did not start"; cat "$W/fakecam.log"; exit 2; }
CAM_ADDR=$(cat "$W/addr")
MQTT_PORT=$((PORT_BASE - 1))
podman run -d --rm --label "$TAG" --name "$TAG-broker" --network host "$BROKER" \
  mosquitto -p "$MQTT_PORT" >/dev/null
for _ in $(seq 100); do podman logs "$TAG-broker" 2>&1 | grep -q running && break; sleep 0.1; done

stat_of() { # stat_of logins|logouts
  sed -n "s/.*\"$1\":\([0-9]*\).*/\1/p" "$W/stats.json"
}

pass() { PASSES=$((PASSES + 1)); echo "PASS $*"; }
fail() { FAILS=$((FAILS + 1)); echo "FAIL $*"; }

# A config in its own directory (mounted as /etc/neolink.toml by the cases); a mains
# camera (battery = false) so that it logs in at startup and must log out on stop. The
# MQTT broker is local: "MQTT Starting" shows the MQTT mode ran.
new_case() { # new_case NAME -> sets C (dir), PORT, NAME; returns 1 to skip
  NAME=$1
  CASE_N=$((CASE_N + 1))
  if [ -n "$ONLY" ] && ! [[ $NAME =~ $ONLY ]]; then return 1; fi
  PORT=$((PORT_BASE + 2 * CASE_N))
  C="$W/case$CASE_N"
  mkdir -p "$C"
  printf 'bind = "127.0.0.1"\nbind_port = %s\nsnapshot_port = 0\n[mqtt]\nbroker_addr = "127.0.0.1"\nport = %s\n[[cameras]]\nname = "fake"\nusername = "admin"\npassword = "fakepass"\naddress = "%s"\nbattery = false\n' \
    "$PORT" "$MQTT_PORT" "$CAM_ADDR" >"$C/neolink.toml"
  chmod 0644 "$C/neolink.toml"
  CNAME="$TAG-$CASE_N"
}

RTSP='Starting RTSP Server'
MQTT='fake: MQTT Starting'

# run_exit WANT_CODE WANT_REGEX DENY_REGEX -- podman-run-args...
# A container that exits by itself: its exit code and output
run_exit() {
  local want_code=$1 want=$2 deny=$3
  shift 4
  local out rc=0
  out=$(timeout -k 10 60 podman run --name "$CNAME" --label "$TAG" --network host "$@" 2>&1) || rc=$?
  podman rm -f "$CNAME" >/dev/null 2>&1 || true
  local why=""
  [ "$rc" = "$want_code" ] || why="exit code $rc, want $want_code; "
  if [ -n "$want" ] && ! grep -qE -- "$want" <<<"$out"; then why+="no /$want/; "; fi
  if [ -n "$deny" ] && grep -qE -- "$deny" <<<"$out"; then why+="has /$deny/; "; fi
  if [ -z "$why" ]; then
    pass "$NAME (exit $rc)"
  else
    fail "$NAME: $why"
    sed 's/^/    | /' <<<"$out" | tail -8
  fi
}

# serve WANT_REGEXES DENY_REGEX -- podman-run-args...
# A long-running container: waits for every regex in WANT_REGEXES ('|||'-separated) and
# for the camera to log in, checks DENY_REGEX is absent, then stops it with STOP (stop:
# SIGTERM via `podman stop -t STOP_T`, or int: SIGINT) and wants exit code WANT_CODE
# (default 0) within MAX_MS (default 5000) and the camera logged out.
serve() {
  local want=$1 deny=$2
  shift 3
  local stop=${STOP:-stop} stop_t=${STOP_T:-10} want_code=${WANT_CODE:-0} max_ms=${MAX_MS:-5000}
  local logins0 logouts0 why="" logs
  logins0=$(stat_of logins)
  logouts0=$(stat_of logouts)
  local err
  if ! err=$(podman run -d --name "$CNAME" --label "$TAG" --network host "$@" 2>&1); then
    podman rm -f "$CNAME" >/dev/null 2>&1 || true
    fail "$NAME: podman run failed: $(tail -1 <<<"$err")"
    return 0
  fi
  IFS=$'\x1f' read -r -a wants <<<"${want//|||/$'\x1f'}"
  local ok=0
  for _ in $(seq 300); do
    logs=$(podman logs "$CNAME" 2>&1 || true)
    ok=1
    for w in "${wants[@]}"; do grep -qE -- "$w" <<<"$logs" || ok=0; done
    [ "$(stat_of logins)" -gt "$logins0" ] || ok=0
    [ $ok = 1 ] && break
    if [ "$(podman inspect "$CNAME" --format '{{.State.Running}}')" != true ]; then break; fi
    sleep 0.1
  done
  [ $ok = 1 ] || why+="did not serve (markers or camera login missing within 30 s); "
  sleep 1
  logs=$(podman logs "$CNAME" 2>&1 || true)
  if [ -n "$deny" ] && grep -qE -- "$deny" <<<"$logs"; then why+="has /$deny/; "; fi
  [ -n "${CHECK_RUNNING:-}" ] && { eval "$CHECK_RUNNING" || why+="running check failed; "; }
  local start ms code
  start=$(date +%s%N)
  if [ "$stop" = int ]; then
    podman kill -s INT "$CNAME" >/dev/null
    timeout -k 5 30 podman wait "$CNAME" >/dev/null || true
  else
    timeout -k 5 60 podman stop -t "$stop_t" "$CNAME" >/dev/null || true
  fi
  ms=$((($(date +%s%N) - start) / 1000000))
  code=$(podman inspect "$CNAME" --format '{{.State.ExitCode}}')
  [ "$code" = "$want_code" ] || why+="exit code $code, want $want_code; "
  [ "$ms" -lt "$max_ms" ] || why+="stop took $ms ms, want < $max_ms; "
  local logins logouts
  for _ in $(seq 30); do
    logins=$(stat_of logins)
    logouts=$(stat_of logouts)
    [ $((logouts - logouts0)) -ge $((logins - logins0)) ] && break
    sleep 0.1
  done
  if [ "$REQUIRE_LOGOUT" = 1 ] && [ "$want_code" = 0 ] \
    && [ $((logouts - logouts0)) -lt $((logins - logins0)) ]; then
    why+="camera not logged out ($((logins - logins0)) logins, $((logouts - logouts0)) logouts); "
  fi
  logs=$(podman logs "$CNAME" 2>&1 || true)
  podman rm -f "$CNAME" >/dev/null 2>&1 || true
  if [ -z "$why" ]; then
    pass "$NAME (stopped in $ms ms, exit $code)"
  else
    fail "$NAME: $why"
    sed 's/^/    | /' <<<"$logs" | grep -v 'Available at' | tail -8
  fi
}

cfg() { echo "$C/neolink.toml:/etc/neolink.toml:ro,z"; }

# helper_sh VOLUME SCRIPT: runs a shell script with the volume at /s
helper_sh() {
  podman run --rm --label "$TAG" -v "$1":/s:z --entrypoint /bin/sh "$HELPER" -c "$2"
}

###########################################################################################
# Command forms

if new_case "default CMD (rtsp)"; then
  serve "$RTSP" "$MQTT" -- -v "$(cfg)" "$IMAGE"
fi
if new_case "command: neolink rtsp --config=/etc/neolink.toml"; then
  serve "$RTSP" "$MQTT" -- -v "$(cfg)" "$IMAGE" neolink rtsp --config=/etc/neolink.toml
fi
if new_case "command: /usr/local/bin/neolink mqtt-rtsp --config ..."; then
  serve "$RTSP|||$MQTT" "" -- -v "$(cfg)" "$IMAGE" \
    /usr/local/bin/neolink mqtt-rtsp --config /etc/neolink.toml
fi
if new_case "command: rtsp --config ..."; then
  serve "$RTSP" "$MQTT" -- -v "$(cfg)" "$IMAGE" rtsp --config /etc/neolink.toml
fi
if new_case "NEO_LINK_MODE=mqtt, default CMD"; then
  serve "$MQTT" "$RTSP" -- -e NEO_LINK_MODE=mqtt -v "$(cfg)" "$IMAGE"
fi
if new_case "NEO_LINK_MODE=mqtt-rtsp, default CMD"; then
  serve "$RTSP|||$MQTT" "" -- -e NEO_LINK_MODE=mqtt-rtsp -v "$(cfg)" "$IMAGE"
fi
if new_case "NEO_LINK_MODE=rtsp, default CMD"; then
  serve "$RTSP" "$MQTT" -- -e NEO_LINK_MODE=rtsp -v "$(cfg)" "$IMAGE"
fi
if new_case "NEO_LINK_MODE=mqtt, command: rtsp --config ... (the subcommand wins)"; then
  serve "$RTSP" "$MQTT" -- -e NEO_LINK_MODE=mqtt -v "$(cfg)" "$IMAGE" rtsp --config /etc/neolink.toml
fi
if new_case "NEO_LINK_MODE=mqtt, command: neolink --config ... (mode applies)"; then
  serve "$MQTT" "$RTSP" -- -e NEO_LINK_MODE=mqtt -v "$(cfg)" "$IMAGE" neolink --config /etc/neolink.toml
fi
if new_case "NEO_LINK_MODE=mqtt, --config rtsp (a file named like a subcommand)"; then
  serve "$MQTT" "$RTSP" -- -e NEO_LINK_MODE=mqtt -w /cfg -v "$C/neolink.toml:/cfg/rtsp:ro,z" "$IMAGE" --config rtsp
fi
if new_case "config path with spaces"; then
  serve "$RTSP" "" -- -v "$C/neolink.toml:/etc/my neolink/neo link.toml:ro,z" "$IMAGE" \
    --config "/etc/my neolink/neo link.toml"
fi
if new_case "config file named neolink, -c /cfg/neolink"; then
  serve "$RTSP" "" -- -v "$C/neolink.toml:/cfg/neolink:ro,z" "$IMAGE" -c /cfg/neolink
fi
if new_case "--help"; then
  run_exit 0 'Usage: neolink' '' -- "$IMAGE" --help
fi
if new_case "--version"; then
  run_exit 0 '^neolink [0-9]' '' -- "$IMAGE" --version
fi
if new_case "help rtsp"; then
  run_exit 0 'Usage: neolink rtsp' '' -- "$IMAGE" help rtsp
fi
if new_case "typo: rstp --config ... (clear error)"; then
  run_exit 2 "unrecognized subcommand 'rstp'" '' -- -v "$(cfg)" "$IMAGE" rstp --config /etc/neolink.toml
fi

if [ $SHELL_IMAGE = 1 ]; then
  if new_case "command: sh -c \"... && exec neolink rtsp ...\""; then
    serve "from-sh|||$RTSP" "" -- -v "$(cfg)" "$IMAGE" \
      sh -c 'echo from-sh && exec neolink rtsp --config /etc/neolink.toml'
  fi
  if new_case "command: bash -lc \"...; exec /usr/local/bin/neolink --config ...\" (NEO_LINK_MODE)"; then
    serve "from-bash|||$MQTT" "$RTSP" -- -e NEO_LINK_MODE=mqtt -v "$(cfg)" "$IMAGE" \
      bash -lc 'echo from-bash; exec /usr/local/bin/neolink --config /etc/neolink.toml'
  fi
  if new_case "command: /bin/sh /custom/script.sh"; then
    printf '#!/bin/sh\necho from-script "$@"\nexec neolink rtsp --config /etc/neolink.toml\n' >"$C/script.sh"
    serve "from-script a b|||$RTSP" "" -- -v "$(cfg)" -v "$C/script.sh:/custom/script.sh:ro,z" \
      "$IMAGE" /bin/sh /custom/script.sh a b
  fi
  if new_case "command: /custom/neolink-start.sh (an executable script, 'neolink' in its name)"; then
    printf '#!/bin/sh\necho from-start-script\nexec neolink --config /etc/neolink.toml\n' >"$C/start.sh"
    chmod 0755 "$C/start.sh"
    serve "from-start-script|||$RTSP" "" -- -v "$(cfg)" -v "$C/start.sh:/custom/neolink-start.sh:ro,z" \
      "$IMAGE" /custom/neolink-start.sh
  fi
  if new_case "kubernetes/manifest.yaml form: sh -c \"cp ... && exec neolink rtsp ...\""; then
    serve "$RTSP" "" -- -v "$C:/config:ro,z" "$IMAGE" sh -c \
      'cp /config/neolink.toml /etc/neolink.toml && exec /usr/local/bin/neolink rtsp --config /etc/neolink.toml'
  fi
  if new_case "sh -c without exec: SIGTERM cannot reach neolink (killed after -t)"; then
    # The shell is PID 1 and has no SIGTERM handler, so the kernel drops the signal: the
    # container is killed at the timeout. Documented: end such commands with `exec`.
    STOP_T=2 WANT_CODE=137 MAX_MS=6000 serve "$RTSP" "" -- -v "$(cfg)" "$IMAGE" sh -c \
      'cp /etc/neolink.toml /tmp/n.toml && /usr/local/bin/neolink rtsp --config /tmp/n.toml'
  fi
  if new_case "--entrypoint /bin/sh -c 'neolink --version'"; then
    run_exit 0 '^neolink [0-9]' '' -- --entrypoint /bin/sh "$IMAGE" -c 'neolink --version'
  fi
  # (--entrypoint drops the image's CMD, so these give a command)
  if new_case "--entrypoint /entrypoint.sh (the old entrypoint), command: --config ..."; then
    serve "$RTSP" "" -- -v "$(cfg)" --entrypoint /entrypoint.sh "$IMAGE" --config /etc/neolink.toml
  fi
  if new_case "--entrypoint /entrypoint.sh, command: neolink mqtt --config ..."; then
    serve "$MQTT" "$RTSP" -- -v "$(cfg)" --entrypoint /entrypoint.sh "$IMAGE" neolink mqtt --config /etc/neolink.toml
  fi
  if new_case "users is a neolink subcommand, not coreutils' users"; then
    run_exit 2 "Usage: neolink users" '' -- -v "$(cfg)" "$IMAGE" users --config /etc/neolink.toml
  fi
  if new_case "command: sh -c 'exit 7' (the command's exit code)"; then
    run_exit 7 '' '' -- "$IMAGE" sh -c 'exit 7'
  fi
else
  if new_case "command: sh -c ... on an image without a shell (clear error)"; then
    run_exit 2 'no program of that name in PATH' '' -- -v "$(cfg)" "$IMAGE" sh -c 'neolink rtsp'
  fi
  if new_case "command: /bin/sh -c ... on an image without a shell (clear error)"; then
    run_exit 127 'could not run "/bin/sh"' '' -- -v "$(cfg)" "$IMAGE" /bin/sh -c 'neolink rtsp'
  fi
  if new_case "native image: CA certificates present"; then
    out=$(podman create --label "$TAG" --name "$CNAME" "$IMAGE")
    if podman cp "$CNAME:/etc/ssl/certs/ca-certificates.crt" "$C/ca.crt" >/dev/null 2>&1 \
      && grep -q 'BEGIN CERTIFICATE' "$C/ca.crt"; then
      pass "$NAME"
    else
      fail "$NAME"
    fi
    podman rm -f "$CNAME" >/dev/null
  fi
fi

###########################################################################################
# Signals and PID 1

if new_case "SIGINT"; then
  STOP=int serve "$RTSP" "" -- -v "$(cfg)" "$IMAGE"
fi
if new_case "--init (catatonit/tini as PID 1 forwards SIGTERM)"; then
  serve "$RTSP" "" -- --init -v "$(cfg)" "$IMAGE"
fi
if new_case "neolink is PID 1"; then
  CHECK_RUNNING='[ "$(podman top "$CNAME" pid comm 2>/dev/null | awk "\$1==1{print \$2}")" = neolink ]' \
    serve "$RTSP" "" -- -v "$(cfg)" "$IMAGE"
fi

###########################################################################################
# Privileges and the state directory

if new_case "default user, state in the /var/lib/neolink volume"; then
  serve "$RTSP" "not writable|could not save" -- -v "$(cfg)" "$IMAGE"
fi
if new_case "PUID=1000 PGID=1000"; then
  serve "$RTSP|||Running as uid 1000, gid 1000" "not writable|could not save" -- \
    -e PUID=1000 -e PGID=1000 -v "$(cfg)" "$IMAGE"
fi
if new_case "PUID=1000 with a root-only (0600) config"; then
  chmod 0600 "$C/neolink.toml"
  podman unshare chown 0:0 "$C/neolink.toml"
  serve "$RTSP|||Running as uid 1000" "not writable|could not save" -- \
    -e PUID=1000 -e PGID=1000 -v "$(cfg)" "$IMAGE"
fi
if new_case "--user 1000:1000 with the image's root-owned state volume (says so)"; then
  serve "$RTSP|||not writable by uid 1000" "" -- --user 1000:1000 -v "$(cfg)" "$IMAGE"
fi
if new_case "--user 1000:1000 with a state directory it owns"; then
  # A bind mount: an empty named volume would be given the image directory's owner
  mkdir "$C/state"
  podman unshare chown 1000:1000 "$C/state"
  serve "$RTSP" "not writable|could not save" -- --user 1000:1000 \
    -v "$C/state:/var/lib/neolink:z" -v "$(cfg)" "$IMAGE"
fi
if new_case "--read-only with a state volume (podman's /tmp tmpfs)"; then
  serve "$RTSP" "not writable|could not save" -- --read-only -v "$(cfg)" "$IMAGE"
fi
if new_case "--read-only without any tmpfs (as docker does it)"; then
  serve "$RTSP" "not writable|could not save" -- --read-only --read-only-tmpfs=false -v "$(cfg)" "$IMAGE"
fi
if new_case "root-owned state volume from an earlier root run, then PUID=1000"; then
  podman volume create --label "$TAG" "$CNAME-state" >/dev/null
  # The earlier run, as root: writes camera/<id>.json and the cache as root
  serve "$RTSP" "" -- -v "$CNAME-state:/var/lib/neolink:z" -v "$(cfg)" "$IMAGE"
  NAME="$NAME (second run)"
  serve "$RTSP|||Running as uid 1000" "not writable|could not save" -- -e PUID=1000 -e PGID=1000 \
    -v "$CNAME-state:/var/lib/neolink:z" -v "$(cfg)" "$IMAGE"
  owners=$(helper_sh "$CNAME-state" 'find /s -path /s/lost+found -prune -o -print0 | xargs -0 stat -c "%u %n"')
  if grep -q '^0 ' <<<"$owners" || ! grep -q 'camera/fake-.*\.json$' <<<"$owners"; then
    fail "$NAME: files left to root or no camera state"; sed 's/^/    | /' <<<"$owners"
  else
    pass "$NAME: every state file belongs to 1000"
  fi
fi
if new_case "NEOLINK_STATE_DIR=/ with PUID=1000 does not hand / over"; then
  # Only an image with a shell can show the owner of / from inside
  [ $SHELL_IMAGE = 1 ] && export CHECK_RUNNING='[ "$(podman exec "$CNAME" stat -c %u /)" = 0 ]'
  serve "$RTSP|||not changing its owner" "" -- -e NEOLINK_STATE_DIR=/ -e PUID=1000 -e PGID=1000 \
    -v "$(cfg)" "$IMAGE"
  unset CHECK_RUNNING
fi

###########################################################################################
# Config errors: a clear message and a non-zero exit, so a restart loop shows the cause

if new_case "missing config"; then
  run_exit 1 'Failed to read "/etc/neolink.toml"' '' -- "$IMAGE"
fi
if new_case "invalid TOML"; then
  echo 'this is = = not toml' >"$C/neolink.toml"
  run_exit 1 'Failed to parse the "/etc/neolink.toml" config file' '' -- -v "$(cfg)" "$IMAGE"
fi
if new_case "unreadable config (--user 1000, file 0600 root)"; then
  chmod 0600 "$C/neolink.toml"
  podman unshare chown 0:0 "$C/neolink.toml"
  run_exit 1 'Permission denied' '' -- --user 1000:1000 -v "$(cfg)" "$IMAGE"
fi
if new_case "invalid PUID"; then
  run_exit 1 'PUID must be a number' '' -- -e PUID=abc -v "$(cfg)" "$IMAGE"
fi

echo
echo "$PASSES passed, $FAILS failed ($IMAGE)"
[ "$FAILS" = 0 ]
