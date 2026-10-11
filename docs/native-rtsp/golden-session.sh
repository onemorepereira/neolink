#!/bin/sh
# Golden re-record against the GStreamer backend (PLAN 0.3b).
# Runs inside neolink-dev with --network host, the repo at /src and ~/.config/neolink at
# /cfg (ro). Needs /target/release/{neolink,xtask} and ffmpeg on PATH.
# Usage: WORK=/scratch/golden sh docs/native-rtsp/golden-session.sh
set -u
OUT=$WORK/raw
mkdir -p $OUT
X=/target/release/xtask
N=/target/release/neolink
umask 077
# Configs derived from the owner's config (password stays local)
sed 's/^bind = .*/bind = "127.0.0.1"\nbind_port = 8554/' /cfg/argus.toml > $WORK/open.toml
awk '/^\[\[cameras\]\]/{print "[[users]]\nname = \"alice\"\npass = \"alicepw\"\n[[users]]\nname = \"bob\"\npass = \"bobpw\"\n"} {print} END{print "permitted_users = [\"alice\"]"}' $WORK/open.toml > $WORK/auth.toml

start_neolink() {
  RUST_LOG=info $N rtsp --config "$1" > $WORK/neolink_$2.log 2>&1 &
  NL=$!
  i=0
  until grep -q "argus: Available at.*sub" $WORK/neolink_$2.log && grep -q "argus: Available at.*main" $WORK/neolink_$2.log; do
    i=$((i+1)); [ $i -gt 90 ] && { echo "neolink not ready"; cat $WORK/neolink_$2.log; kill $NL; exit 1; }
    sleep 1
  done
  echo "neolink ready ($2) after ${i}s"
}

# name url-path transport [creds]
rec() {
  name=$1; path=$2; transport=$3; creds=${4:-}
  $X golden record --listen 127.0.0.1:8555 --upstream 127.0.0.1:8554 --out $OUT/$name.txt 2>/dev/null &
  PX=$!
  sleep 0.3
  if [ "$transport" = tcp ]; then T="-rtsp_transport tcp"; else T=""; fi
  timeout 30 ffmpeg -v error -nostdin $T -i "rtsp://${creds}127.0.0.1:8555$path" -t 4 -f null - > $OUT/$name.ffmpeg.log 2>&1
  echo "$name: ffmpeg exit $? ($(head -c 200 $OUT/$name.ffmpeg.log | tr '\n' ' '))"
  wait $PX
}

date -u +"open start %T"
start_neolink $WORK/open.toml open
for a in main Main mainStream MainStream Mainstream mainstream sub Sub subStream SubStream Substream substream extern Extern externStream ExternStream Externstream externstream; do
  rec open_${a}_tcp /argus/$a tcp
done
rec open_bare_tcp /argus tcp
rec open_main_tcp_repeat /argus/main tcp
rec open_main_udp /argus/main udp
rec open_sub_udp /argus/sub udp
rec open_unknown_path /nope/main tcp
kill -TERM $NL; wait $NL  # sh ignores SIGINT in background jobs
date -u +"open end %T"

start_neolink $WORK/auth.toml auth
rec auth_none /argus/main tcp
rec auth_wrong_pass /argus/main tcp alice:wrong@
rec auth_unknown_user /argus/main tcp mallory:x@
rec auth_not_permitted /argus/main tcp bob:bobpw@
rec auth_ok /argus/main tcp alice:alicepw@
kill -TERM $NL; wait $NL  # sh ignores SIGINT in background jobs
date -u +"auth end %T"
