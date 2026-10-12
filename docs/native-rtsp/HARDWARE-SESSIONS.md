# Hardware sessions for the native backend (Argus PT Ultra)

The remaining gates need the real camera. Rules (PLAN "Hardware budget"):
- **Limits:** at most 2 sessions a day, each at most 10 min of streaming, and only with the
  battery at 40 % or more.
- **Battery:** log the level before and after each session.
- **Interference:** don't open the Reolink app around a session (it wakes the camera).

Common setup, on the neolink host (local network only):

```sh
# The native image (Dockerfile.native) or the binary; the camera config is the existing
# ~/.config/neolink/argus.toml plus:  rtsp_backend = "native"
podman run -d --name neolink-native --network host \
  -v ~/.config/neolink/argus.toml:/etc/neolink.toml:ro,z \
  -v neolink-state:/var/lib/neolink neolink:native
podman logs -f neolink-native     # the wake ledger and battery readings are logged here
neolink battery --fresh --config ~/.config/neolink/argus.toml   # before/after (one wake)
```

## Results so far

**Session 1, 2026-10-12 00:17 UTC** (battery 50 %, 3687 mV, −847 mA, not charging):
- **A1:** main H.265 3840×2160 at 15 fps with AAC; sub H.264 896×512 at 15 fps with AAC.
  Both measured 15.2 fps.
- **A2:**
  - sub: 60 s, 901 frames, 0 decode errors;
  - main: 120 s, 1801 frames, 0 decode errors, no `RTP: missed` or `PPS id`, audio and
    video start together.
- **A7:**
  - the two main clients that opened during the first wake ever (no cache) got 503
    `Retry-After: 10`, as R11 says. The camera took 6 s to wake from deep sleep, more than
    DESCRIBE's 2 s;
  - the later main clients and the sub client played; sub learned in the same wake.
- **Power:** one wake per part (108 s and 138 s awake); asleep 10 s after the last client.
- **N1 asleep** (desc cached): first live frame 4.2 s after DESCRIBE.

**Session 2, 2026-10-12 00:42 UTC** (battery 49 %, 3677 mV):
- **A5:** VLC 300 s, 4337 frames displayed, 0 lost, 0 "picture is too late", audio decoded.
- **5.1:** Frigate's preset-rtsp-generic for 290 s: exit 0, 29 of 29 segments. The only
  message is the startup note "Non-monotonic DTS; previous: 0, current: 0", the same as on
  fixtures; GStreamer is not compared on hardware yet.
- **A4:** go2rtc frame.jpeg from main (689 KB) and sub.
- **A4b:** the re-publish path returns a JPEG.
- **Power:** one wake, 350 s awake for a 5.7 min session.
- **Cache:** the persisted keyframe loaded (228 KB) and went to the first client.

## Session 1 (day 1): A1, A2, A7, on main and sub

1. A1:
   - `ffprobe -rtsp_transport tcp rtsp://HOST:8554/argus/main`, then the same for `/sub`;
   - expect H.265 4K (3840×2160) on main and H.264 on sub, matching the Reolink app's stream info, with AAC on both and fps ±5 %.
2. A2: `ffmpeg -rtsp_transport tcp -i …/main -t 120 -c copy main.mkv`, then decode with
   `ffmpeg -v error -i main.mkv -f null -`. Expect 0 error lines, and no `RTP: missed` or
   `PPS id out of range`.
3. A7:
   - start 4 clients on main at staggered times, the first while the camera is waking;
   - add one sub client alongside;
   - expect 1 camera stream start per kind in the log, and every client's first frame
     decodes.
4. N1 (awake): note the time from DESCRIBE to the first frame.
5. Stop all clients; the camera must disconnect `idle_grace_s` (10 s) after the last one
   leaves (ledger).

## Session 2 (day 1): A4, A5

1. A4: go2rtc pulls `rtsp://HOST:8554/argus/main`; `GET /api/frame.jpeg?src=…` returns a
   JPEG.
2. A4b: run `ffmpeg -rtsp_transport tcp -i …/main -c copy -f rtsp rtsp://GO2RTC/republish`,
   then frame.jpeg from that stream (the Home Assistant path, under the default
   `parameter_sets = "strip"`).
3. A5: `cvlc --rtsp-tcp rtsp://HOST:8554/argus/main` for 5 min. Expect fewer than one
   "picture is too late" per minute.
4. Also: `tests/e2e/frigate.sh`'s generic preset command line against the camera for the
   rest of the 10 min (PLAN 5.1, hardware part). Check whether "Non-monotonic DTS;
   previous: 0, current: 0" appears, as it does on fixtures.

## Session 3 (day 2): A11, outage

1. Play main with ffmpeg (`-c copy` to a file), VLC and go2rtc together.
2. After 2 min, drop the camera for 60 s on the neolink host. This needs root, so you run
   it: `sudo nft add table inet a11; sudo nft add chain inet a11 o '{ type filter hook output priority 0; }'; sudo nft add rule inet a11 o ip daddr 192.168.86.34 drop`.
   After 60 s: `sudo nft delete table inet a11`.
3. Expect:
   - every client keeps its session (bridged frames during the outage);
   - live media resumes from a keyframe;
   - the recording's timestamps are monotonic with no jump over 1 s.

## Session 4 (day 2): A12 with you at the PIR, plus N1 asleep

`pause.on_motion` can't start a sleeping camera on motion: without push notifications,
motion arrives only over an open connection (NATIVE.md). The hardware A12 therefore runs
with the camera kept connected, for this session only:

1. Set `idle_disconnect = false` and `[cameras.pause] on_motion = true`, then restart.
2. Play main with ffmpeg; nothing moves: no camera stream (log), the session stays up.
3. You walk in front of the PIR: the stream starts within 2 RTT + 1 GOP of the motion
   event; you leave: it pauses again.
4. Restore the config (sleeping camera), wait 10 min, then play: N1 asleep is wake time
   + about 2 s to the first live frame. The cached keyframe shows at once.

## Also pending

- The Phase 1 deferred check: 10 connect/teardown cycles with 6 s gaps on the camera.
  Expect one wake per cycle (or one held wake within `idle_grace_s`), and the camera
  asleep after the last.
- 5.3: VLC and retina timeline checks (fits into session 2 or 3).
- Gate 6 fresh-machine test: on a clean machine, take the static binary, write a config,
  run it, point Frigate's preset command line at it. No other install steps.
- 24 h soak: hardware only with mains power, so deferred (no mains camera).
