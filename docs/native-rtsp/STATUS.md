# Native RTSP backend: where it stands (2026-10-12)

## In one paragraph

Neolink has a GStreamer-free RTSP server (`crates/rtsp`) wired into neolink behind the
`native-rtsp` feature (`rtsp_backend = "native"`). The native build is one static binary:
- x86_64: 14 MB;
- aarch64: 11.6 MB, built but not yet run;
- a 14.3 MB `FROM scratch` image.

It needs no GStreamer, plugins or OpenSSL. It is built for battery cameras that must sleep
when nobody watches. Phases 0–6 of [PLAN.md](PLAN.md) are done in code and CI on fixtures.
Four hardware sessions have run on an Argus PT Ultra. The GStreamer backend is still the
default (beta).

## What works, with evidence

- **Clients**, on fixtures, through neolink and a scripted camera:
  - ffmpeg/ffprobe, plain ffmpeg (UDP, then 461 and TCP);
  - go2rtc 1.9.14, direct and via Home Assistant's re-publish path;
  - VLC 3.0.24;
  - retina 0.4.21;
  - Frigate 0.14's preset command lines, 15 min across the camera-clock wrap.
- **Against GStreamer:** a golden diff of all 28 recorded cases differs only by the
  documented changes (SPEC §5).
- **A/V sync:** |A−V| ≤ 74 ms over 15 min (GStreamer started them 1.9 s apart).
- **Performance:**
  - 4K H.265 to 4 clients uses 1.98 % of one core;
  - 58 KiB per added client (measured before the N4 method fix);
  - a 1 h soak keeps RSS within ±4 %.
- **Security:**
  - Digest and Basic auth, re-authorization on every request;
  - connection, rate and failed-auth limits;
  - no credentials in logs;
  - fuzzed parsers.
  - Three red-team reviews ran (correctness, security, full branch), and their findings
    are fixed.
- **Power:**
  - no contact at boot;
  - DESCRIBE answered from a cache, so health probes don't wake the camera;
  - one wake per viewing session, released `idle_grace_s` after the last client;
  - learning wakes limited per camera.
- **Hardware** (2026-10-12, battery 50 % → 45 %; details in
  [HARDWARE-SESSIONS.md](HARDWARE-SESSIONS.md)):
  - A1, A2, A7: 4K H.265 main and H.264 sub, 0 decode errors;
  - A4, A4b: go2rtc snapshots and the Home Assistant re-publish path;
  - A5: VLC 5 min, 0 lost;
  - Frigate's preset on the camera;
  - A11: a 60 s outage is bridged, every client keeps its session, live video resumes
    on a keyframe;
  - the 10-cycle check;
  - A12: motion starts and pauses the stream.

## What's next

1. **A short repeat of hardware session 4**, with the cold-start bridging fix in, to confirm
   it on the camera and to measure motion → first live keyframe (A12 asks ≤ 2 RTT + 1 GOP).
   Battery ≥ 40 %; the camera stays connected for it.
2. **Gate 6 fresh-machine test:** on a clean machine, download the static binary, write a
   config, run it, and point Frigate's preset command line at it. No other install steps.
3. **The aarch64 binary on real arm64 hardware**, e.g. a Raspberry Pi. It is built but has
   never run.
4. **Open questions:**
   - Frigate's startup note "Non-monotonic DTS; previous: 0, current: 0" appears on the
     native backend but not on GStreamer (fixtures, and on the camera). ffmpeg corrects it
     and recordings are fine, but it is not understood yet;
   - why one restarted run in session 1 didn't send the persisted keyframe (it did in
     session 2; debug logging is now in place);
   - VLC drops a few frames as "too late" right after an outage ends (0.2 fps bridging);
   - `pause.on_motion` can't start a sleeping camera: motion events only arrive over an
     open connection, and push notifications (cloud) are left out of the local-only build.
5. **Later (PLAN Phase 7):** make native the default once it has run longer on more
   cameras. That needs a 24 h hardware soak, which needs mains power, and the 90 min
   nightly runs on a CI runner.

## Where things are

- **Branches:** `master` holds everything merged. `feature/native-rtsp` is the integration
  branch, and the phase branches (`-p2` … `-p5`, `-hw`) are kept for history.
- **Docs:**
  - [SPEC.md](SPEC.md), [DESIGN.md](DESIGN.md), [PLAN.md](PLAN.md) (per-phase status and
    gate evidence);
  - [NATIVE.md](NATIVE.md), the user guide;
  - [HARDWARE-SESSIONS.md](HARDWARE-SESSIONS.md), the hardware plan and results.
- **Tests:** `cargo test --workspace --features native-rtsp`, plus `tests/e2e/*.sh`:
  - `clients.sh`, `golden-native.sh`, `avsync.sh`, `frigate.sh`;
  - `soak.sh`, `perf.sh`, `container-native.sh`;
  - with `SERVER=neolink` for the real neolink path.
- **CI:** `.github/workflows/test.yml`, `nightly.yml`, `native-release.yml` (artifacts only).
