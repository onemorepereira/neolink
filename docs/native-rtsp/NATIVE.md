# The native RTSP backend (beta)

Neolink can serve RTSP without GStreamer. The native backend is a single static binary:
no GStreamer, no plugins, no OpenSSL, nothing to install. It is in beta: the GStreamer
backend stays the default until the native one has run on more cameras (SPEC §8).

## Install

**Binary.** Download the native build for your platform from the release artifacts:
- `neolink-native-x86_64-linux-musl`
- `neolink-native-aarch64-linux-musl` (Raspberry Pi 4/5 and other arm64 boards)

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
  `PUID`/`PGID` to drop to that user once the config is read. The state volume must then
  be writable by that user.

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
| `limits.*` | top | see below | `max_connections` 128, `max_unauthenticated_per_ip` 64, `first_request_timeout_s` 10, `idle_timeout_s` 60, `requests_per_second` 20, `failed_auth_per_minute` 10 |
| `mqtt_config_writes` | top | `true` | Set `false` to stop MQTT clients rewriting the config |
| `audio` | camera | `aac` | `aac` or `none` |
| `pacing_latency_ms` | camera | 1500 | 0–3000: the jitter buffer that smooths the camera's bursty Wi-Fi delivery |
| `parameter_sets` | camera | `strip` | `strip`, `passthrough` or `inject`. Keep `strip` if you re-publish to go2rtc (Home Assistant): go2rtc 1.9.14 can't read ffmpeg's re-published in-band parameter sets |
| `bridge_fps` | camera | 0.2 | 0.1–2: frames per second sent during a camera outage |
| `bridge_max_s` | camera | 120 | 10–3600: how long an outage is bridged before sessions end |
| `rtcp_sr` | camera | `true` | RTCP sender reports (A/V sync). Per server for now: on while any camera wants it |
| `persist_irap` | camera | `true` | Keep the last keyframe on disk to show while a sleeping camera wakes |

`bind`, `bind_port`, `limits` and `rtcp_sr` apply at start. Everything else applies
live, without a restart:
- users and permitted users;
- cameras added or removed;
- stream settings (these restart that camera's sessions).

A live reload that adds `certificate` or `tls_client_auth` is refused as a whole: RTSPS is
not supported yet.

## Battery cameras

The native backend is built for cameras that must sleep:
- **No contact at boot.** A sleeping camera is not contacted until a client plays.
- **Probes don't wake it.** After the first time, `DESCRIBE` (health checks, Home
  Assistant probes) is answered from a cache in the state directory.
- **Viewing wakes it.** `PLAY` wakes the camera. While it wakes, the client sees the last
  keyframe. Live video follows when the camera is up: about 1 s while streaming, and
  wake time plus about 5 ms when asleep.
- **It sleeps again soon.** The camera is released `idle_grace_s` after the last viewer
  leaves.
- **One wake per 10 minutes for learning.** A stream that has never been seen is learned
  at most once per camera per 10 minutes. One wake teaches every stream kind.
- `battery` unknown is treated as battery.

`pause.on_motion` needs motion events, and a camera sends those only over an open
connection. On a sleeping camera, motion-gated streaming therefore needs push
notifications, or the camera must stay connected (`idle_disconnect = false`).

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

- **`irap.bin`** (in the state directory, one per camera stream) holds the last keyframe:
  an image of what the camera last saw. Set `persist_irap = false` to keep it only in
  memory, and protect the state directory like the config.
- **MQTT.** The `neolink/config` topic can rewrite the whole config, including users and
  `permitted_users`. Anyone who can publish to your broker can do that. Set
  `mqtt_config_writes = false` unless you use it. A malformed config sent there is
  answered with the line number only, never the line's text.
- **Logs** never contain passwords, `Authorization` headers or `user:pass@` URLs, at any
  level.
- **Bridged media during outages is synthetic** (a repeated keyframe and silence). A
  recording made during an outage shows a still image, not lost footage.

## Tested clients

On synthetic fixtures in CI, and through neolink with a scripted camera:
- ffmpeg/ffprobe, plain ffmpeg (UDP first, then TCP);
- go2rtc 1.9.14 (direct, and the Home Assistant re-publish path);
- VLC 3.0.24;
- retina 0.4.21;
- Frigate 0.14's `preset-rtsp-generic` and `preset-rtsp-restream` command lines (15 min across
  the camera clock wrap, plus a 60 s outage).

Hardware testing on an Argus PT Ultra is in progress (PLAN Phase 4).
