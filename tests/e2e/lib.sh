# Shared helpers for the e2e scripts. Source it right after `set -euo pipefail`:
#   . "$(dirname "$0")/lib.sh"
# Every wait has a deadline and fails the script when it passes, every background process
# is killed when the script exits (pass, fail or interrupted), and a helper that cannot do
# its job exits the script non-zero instead of letting a check pass on missing data.
# shellcheck shell=bash

# Background processes to kill on exit: start one, then `track $!`
E2E_PIDS=()
# Containers to remove on exit (`track_ctr NAME`); E2E_ENGINE picks podman or docker
E2E_CTRS=()
E2E_ENGINE=${E2E_ENGINE:-podman}

track() { E2E_PIDS+=("$1"); }
track_ctr() { E2E_CTRS+=("$1"); }

die() {
  echo "FAIL (aborted) $*" >&2
  exit 1
}

e2e_cleanup() {
  local rc=$? p i alive
  trap - EXIT INT TERM
  for p in "${E2E_PIDS[@]}"; do
    # A tracked subshell (`f &`): its children first (timeout, ffmpeg)
    if command -v pkill >/dev/null; then pkill -TERM -P "$p" 2>/dev/null || true; fi
    kill -TERM "$p" 2>/dev/null || true
  done
  for i in $(seq 50); do
    alive=0
    for p in "${E2E_PIDS[@]}"; do kill -0 "$p" 2>/dev/null && alive=1; done
    [ $alive = 0 ] && break
    sleep 0.1
  done
  for p in "${E2E_PIDS[@]}"; do kill -KILL "$p" 2>/dev/null || true; done
  for p in "${E2E_CTRS[@]}"; do "$E2E_ENGINE" rm -f -t 0 "$p" >/dev/null 2>&1 || true; done
  exit "$rc"
}
trap e2e_cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# wait_until SECS PID WHAT CMD...: run CMD until it succeeds; fail the script after SECS
# seconds, or as soon as PID (if not empty) has exited
wait_until() {
  local secs=$1 pid=$2 what=$3 i=0
  shift 3
  until "$@"; do
    if [ -n "$pid" ] && ! kill -0 "$pid" 2>/dev/null; then die "$what: the process ($pid) exited"; fi
    i=$((i + 1))
    [ $i -gt $((secs * 10)) ] && die "$what: not within ${secs}s"
    sleep 0.1
  done
}

# wait_pid PID SECS WHAT: wait for PID to exit and return its exit status; kill it and
# fail the script if it is still running after SECS seconds
wait_pid() {
  local pid=$1 secs=$2 what=$3 i=0 rc=0
  while kill -0 "$pid" 2>/dev/null; do
    i=$((i + 1))
    if [ $i -gt $((secs * 10)) ]; then
      kill -KILL "$pid" 2>/dev/null || true
      die "$what: still running after ${secs}s"
    fi
    sleep 0.1
  done
  wait "$pid" || rc=$?
  return $rc
}

# stop_pid PID WHAT [WANT_CODES]: SIGTERM a process that must still be running, wait up
# to 10 s, and fail the script if it had already exited (it died during the run), did not
# stop, or exited with a code outside WANT_CODES (space-separated, default "0 143")
stop_pid() {
  local pid=$1 what=$2 want=${3:-0 143} rc=0
  alive "$pid" "$what"
  kill -TERM "$pid" 2>/dev/null || true
  wait_pid "$pid" 10 "$what after SIGTERM" || rc=$?
  case " $want " in *" $rc "*) ;; *) die "$what: exit code $rc after SIGTERM, want one of: $want" ;; esac
}

# alive PID WHAT: fail the script if a process that must run for the whole test (a server,
# the fake camera) has exited; call it before each case so a dead one stops the run at once
alive() {
  local rc=0
  kill -0 "$1" 2>/dev/null && return 0
  wait "$1" || rc=$?
  die "$2 exited during the run (exit $rc)"
}

# need FILE WHAT: fail the script if FILE is missing or empty (data a check relies on)
need() { [ -s "$1" ] || die "$2: $1 is missing or empty"; }

# grep_count PATTERN FILE: number of matching lines (0 when none); fails the script when
# grep itself fails (unreadable file, bad pattern)
grep_count() {
  local n rc=0
  n=$(grep -c -i -E -- "$1" "$2") || rc=$?
  [ $rc -le 1 ] || die "grep failed on $2 (exit $rc)"
  echo "$n"
}
