# The native RTSP backend (beta)

Neolink can serve RTSP without GStreamer. The native backend is a single static binary:
no GStreamer, no plugins, no OpenSSL, nothing to install. It is in beta: the GStreamer
backend stays the default until the native one has run on more cameras (SPEC §8).

## Install

**Binary.** Build it, or take it from the `Native release artifacts` workflow's
artifacts:
- `neolink-native-x86_64-linux-musl`
- `neolink-native-aarch64-linux-musl`, for 64-bit ARM boards. It is not yet tested on
  hardware: it is built, but no arm64 machine was available to run it.

```sh
cargo build --release --no-default-features --features native-rtsp   # x86_64 musl on Alpine
```

Then:

```sh
chmod +x neolink
./neolink rtsp --config neolink.toml
```

The binary is statically linked and runs on any Linux of its architecture.

**Docker.** `Dockerfile.native` builds a `FROM scratch` image of about 15 MB:

```sh
podman build -t neolink:native -f Dockerfile.native .
podman run -d --network host -v ./neolink.toml:/etc/neolink.toml:ro \
  -v neolink-state:/var/lib/neolink neolink:native
```

It keeps the old image's conventions: `NEO_LINK_MODE` (`rtsp`, `mqtt`, `mqtt-rtsp`, …), the
config at `/etc/neolink.toml`, and state in `/var/lib/neolink`. Differences from the
GStreamer image:
- **No shell, no `/entrypoint.sh`.** neolink is PID 1 and stops cleanly on `docker stop`
  (BYE to clients, camera logout, under 1 s).
- **Runs as root by default**, so existing root-owned `0600` configs still work. Set
  `PUID`/`PGID` to drop to that user once the config is read:
  - neolink first hands the state directory to that user;
  - MQTT TLS files (`ca`, `client_auth`) must be readable by it. An unreadable configured
    CA is an error, never a fall back to plaintext;
  - ports below 1024 need root, since the listeners open after the drop.

## Choosing the backend

```toml
rtsp_backend = "native"   # or "gstreamer"
```

or `neolink rtsp --rtsp-backend native`. Without either, a build that has GStreamer uses
it; the native build uses native. Asking for a backend the build doesn't have is a
startup error.

## Configuration

Everything that matters for RTSP still works:
- `bind`, `bind_port`, `users`, `permitted_users` (including `anyone` and `anonymous`);
- `stream`, `enabled`, `channel_id`, `strict`, `pause.on_motion`, `idle_disconnect`;
- all camera connection keys.

The URLs are the same 19 aliases per camera (`/Cam`, `/Cam/main`, `/Cam/mainStream`,
`/Cam/sub`, …).

**New keys:**

| Key | Where | Default | What |
|---|---|---|---|
| `rtsp_backend` | top | GStreamer if built in | `gstreamer` or `native` |
| `rtsp_auth` | top | `["digest", "basic"]` | Schemes offered. Basic sends passwords in clear text (there is no TLS yet), so prefer `["digest"]` when every client supports it |
| `rtp_mtu` | top | 1400 | 576–9000 |
| `limits.*` | top | see below | `max_connections` 128, `max_unauthenticated_per_ip` 32, `first_request_timeout_s` 10, `idle_timeout_s` 60, `requests_per_second` 20, `failed_auth_per_minute` 10. Unauthenticated means no credentials accepted and no session yet; past the per-IP cap (or `max_connections / 2` in all) an old unauthenticated connection is closed, so a flood can't lock others out, and authenticated or playing clients never count. Past `failed_auth_per_minute` wrong passwords for one user from one IP (or 5 × that across all usernames), that user's credentials from that IP get 401, right or wrong, until a minute passes; sessions already playing as that user are not affected. The snapshot endpoint applies the same limits |
| `mqtt_config_writes` | top | `true` | Set `false` to stop MQTT clients rewriting the config |
| `audio` | camera | `aac` | `aac` or `none` |
| `pacing_latency_ms` | camera | 1500 | 0–3000: the jitter buffer that smooths the camera's bursty Wi-Fi delivery |
| `parameter_sets` | camera | `strip` | `strip`, `passthrough` or `inject`. Keep `strip` if you re-publish to go2rtc (Home Assistant): go2rtc 1.9.14 can't read ffmpeg's re-published in-band parameter sets |
| `bridge_fps` | camera | 0.2 | 0.1–2: frames per second sent during a camera outage |
| `bridge_max_s` | camera | 120 | 10–3600: how long an outage is bridged before sessions end |
| `rtcp_sr` | camera | `true` | RTCP sender reports (A/V sync). Per server for now: on while any camera wants it |
| `persist_irap` | camera | `true` | Keep the last keyframe on disk to show while a sleeping camera wakes |

**Changing the config while running.** The only live path is the MQTT `neolink/config`
topic (in memory: the file is not rewritten and there is no file watch). Through it:
- users, permitted users and cameras added or removed apply at once;
- stream settings apply at once, restarting that camera's sessions;
- `rtp_mtu` applies when a camera's streams are next recreated;
- `bind`, `bind_port`, `limits` and `rtcp_sr` need a restart.

A config sent there with `certificate` or `tls_client_auth` is refused as a whole: RTSPS
is not supported yet. Otherwise, edit the file and restart (`docker restart` takes under
a second).

**Power and snapshot settings.** These are shared with the GStreamer backend (see the
README's [Power section](../../README.md#power-sleeping-and-always-on-cameras)):

| Key | Where | Default | What |
|---|---|---|---|
| `battery` | camera | unset, treated as battery | `false` marks a mains camera, which then stays connected. Not detected automatically |
| `idle_disconnect` | camera | unset: follows `battery` | `true` forces sleeping, `false` forces always-on (with a warning unless `battery = false`) |
| `idle_grace_s` | camera | 10 | How long a sleeping camera stays connected after the last use |
| `motion_hold_max_s` | camera | 120 | Longest a motion event keeps the camera awake |
| `snapshot_port` | top | `bind_port + 1` | The snapshot endpoint (below); `0` turns it off |
| `snapshot_fresh` | camera | `false` | Allow `?fresh=1`, which wakes the camera for a new picture |

**A camera with neither `battery` nor `idle_disconnect` set sleeps**, whatever its real
power source, with either backend. Before this work every camera stayed connected. Set
`battery = false` on mains, PoE and NVR-channel cameras to keep that behaviour.

**The snapshot endpoint.** It is on by default at `http://<bind>:<bind_port+1>/<camera>/snapshot.jpg`,
in `rtsp` and `mqtt-rtsp` modes, with either backend.
- It serves the last picture taken at a wake, without waking a sleeping camera. Until the
  first wake it serves a grey placeholder (HTTP 200).
- `?fresh=1` takes a new picture, waking the camera: only with `snapshot_fresh = true`
  (otherwise 403), and at most once a minute per camera.
- With no `[[users]]`, anyone who can reach the port may fetch it. With users, it takes
  HTTP Basic only, even when `rtsp_auth` is digest-only. Failed logins are paced like
  RTSP's.
- Set `snapshot_port = 0` if you don't use it, or if the port clashes: 8555 is also
  go2rtc's WebRTC port. A clash is logged, and RTSP keeps running.

## Battery cameras

A camera sleeps when `idle_disconnect = true`, or when `idle_disconnect` is unset and
`battery` is not `false`, so **by default**. Sleeping itself (no contact at boot, release
after `idle_grace_s`) is the same with both backends. The native backend adds what keeps
probes from waking the camera:
- **No contact at boot.** A sleeping camera is not contacted until a client plays.
- **Probes don't wake it.** After the first time, `DESCRIBE` (health checks, Home
  Assistant probes) is answered from a cache in the state directory.
- **Viewing wakes it.** `PLAY` wakes the camera. While it wakes, the client sees the last
  keyframe. Live video follows when the camera is up: about 1 s while streaming, and
  wake time plus about 5 ms when asleep.
- **It sleeps again soon.** The camera is released `idle_grace_s` after the last viewer
  leaves.
- **One wake per 10 minutes for learning.** A stream that has never been seen is learned
  at most once per camera per 10 minutes. One wake teaches every stream kind. A camera
  that is already connected is not limited, and neither is an always-on camera.
- `battery` unknown is treated as battery: neolink never decides on its own that a camera
  is on mains power.

`pause.on_motion` needs motion events, and a camera sends those only over an open
connection. **On a sleeping camera, `pause.on_motion` cannot start the stream on motion.**
The native build has no push notifications (the optional `pushnoti` feature is not in
it), which would go through Google's and Reolink's cloud. Neolink warns about this at
startup. Use it only on a camera that stays connected (`battery = false` on mains, or
`idle_disconnect = false`, which costs battery), or let clients wake the camera by
playing. The same holds for MQTT motion and floodlight status: they are reported only
while the camera is awake.

## What behaves differently from the GStreamer backend

These are the SPEC §5 deltas:
- **Camera streams.** One camera stream per stream kind, however many clients.
- **Audio.** AAC passes through as is; GStreamer decoded it to L16 stereo. ADPCM cameras
  are served without audio for now (warned once).
- **No silence fill** when the camera sends no audio.
- **Timestamps** come from the camera clock, so there is no wrap after 71 minutes. Audio and
  video start together (GStreamer: about 1.9 s apart), and |A−V| stayed ≤ 74 ms over 15 min.
- **No splash test pattern.** During an outage clients get the last keyframe repeated
  (`bridge_fps`) and silent audio, then BYE after `bridge_max_s`.
- **DESCRIBE answers within 2 s** whatever the camera state (503 with `Retry-After: 10`
  if nothing is known yet).
- **Transport.** UDP is answered with 461, and ffmpeg/VLC/go2rtc then use TCP. RTSPS is not
  supported yet.
- **Auth.** Digest is offered as well as Basic.
- **Session timeout** is 60 s and bound to the TCP connection; keepalives are accepted but
  not needed.
- **SDP.** It says "neolink", adds `a=framerate`, and drops `a=ts-refclk`/`a=mediaclk`/`a=ssrc`.
- **Protocol details.**
  - The `Public` header lists play methods only.
  - A stream the camera doesn't have (e.g. extern on an Argus) answers 404.
  - TEARDOWN closes the connection, as GStreamer does.

## Privacy and trust

- **`irap.bin`** (in the state directory as `cache/<hash>-<stream>/irap.bin`, one per
  camera stream) holds the last keyframe:
  an image of what the camera last saw. Set `persist_irap = false` to keep it only in
  memory, and protect the state directory like the config.
- **MQTT.** The `neolink/config` topic can rewrite the whole config, including users,
  `permitted_users` and camera addresses. Anyone who can publish to your broker can do
  that. Set `mqtt_config_writes = false` unless you use it.
  - Passwords left out of a config sent there are filled in from the running config only
    when the camera's address (and UID) and the MQTT broker are unchanged. A config that
    points a camera or the broker elsewhere doesn't get the current secrets.
  - A malformed config is answered with the line number only, never the line's text.
- **Snapshots.** `camera/<name>-<hash>.jpg` in the state directory (next to that camera's
  `<name>-<hash>.json` state file) is the last picture of each camera; treat it like
  `irap.bin`.
- **Logs** never contain passwords, `Authorization` headers or `user:pass@` URLs, at any
  level.
- **Bridged media during outages is synthetic** (a repeated keyframe and silence). A
  recording made during an outage shows a still image, not lost footage.

## Tested clients

On synthetic fixtures in CI:
- **Through neolink with a scripted camera, and against the server crate alone:**
  ffmpeg/ffprobe, plain ffmpeg (UDP first, then TCP), go2rtc 1.9.14 (direct, and the Home
  Assistant re-publish path), VLC 3.0.24.
- **Against the server crate:** retina 0.4.21, and Frigate 0.14's `preset-rtsp-generic` and
  `preset-rtsp-restream` command lines (15 min across the camera clock wrap, plus a 60 s
  stall of the replayed stream).

Hardware testing on an Argus PT Ultra is in progress (PLAN Phase 4).
