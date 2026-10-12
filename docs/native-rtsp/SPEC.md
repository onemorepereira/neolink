# Native RTSP: Specification

Status: **v2.3** (2026-10-11). v2 was revised after two adversarial reviews (protocol/media and
scope/security/ops); v2.1 adds amendments from the design reviews and the golden transcripts; v2.2 applies the whole-doc-set review; v2.3 records the Phase 0 measurements (§9) and restates N3/N5 for the owner's hardware. Companion docs: [DESIGN.md](DESIGN.md), [PLAN.md](PLAN.md).

Notation: **MUST/SHOULD** per RFC 2119. Requirement IDs are stable across revisions;
removed IDs are not reused.

## 1. Problem

Neolink's RTSP server is built on GStreamer (`gstreamer-rtsp-server` ≥ 1.20, plus
plugins-base/good/bad and gst-libav at runtime). That is the main source of installation
pain:

- Users must install a specific GStreamer version with the right plugins on every OS.
- The Docker image is 727 MB (measured), almost all GStreamer, to wrap one binary.
- Missing plugins (`faad`, `h265parse`, `fallbackswitch`) fail at runtime, not build time.

## 2. Goal

Ship neolink as **one self-contained binary per platform** that serves camera streams over
RTSP with **no runtime system libraries** beyond libc (none for musl builds) and no
build-time system libraries beyond a C compiler (for `ring` and jemalloc).

"Done" means: download one file, write a config, run it, point Frigate, Home Assistant
(go2rtc), Blue Iris, VLC or ffmpeg at it, and it keeps working for days.

## 3. Decision record

**Context.** [bairelay](https://github.com/mgc8/bairelay) (AGPL-3.0-or-later, created
2026-04, one author, AI-assisted) is a pure-Rust continuation of neolink that targets
battery Argus cameras specifically. It ships static musl binaries, an RTSP server
(H.264/H.265/AAC/G.711, TCP+UDP, RTSPS, Basic+Digest), MQTT with Home Assistant discovery,
PTZ/PIR/LED/users/snapshot, a local wake server and push listener, and about 2,200 tests,
8 fuzz targets and a fixture-replay harness. It lacks `talk` and declares non-battery
cameras out of scope. Upstream neolink has had no commits since 2025-01-30.

**Decision (project owner, 2026-10-10): build the native RTSP server inside neolink.**
Adopting bairelay was offered, with a time-boxed evaluation, and declined.

**Consequences.**
- Port and adapt bairelay code where it helps (packetizers, auth, Transport parsing,
  pacer and bridging logic), keeping its AGPL attribution headers. Licenses are compatible.
- Neolink supports wired and NVR-attached cameras as well as battery ones, so nothing here
  may assume battery-only behaviour.
- bairelay's field notes are treated as hypotheses and checked against our own captures
  (§9). For example, bairelay reports Reolink NAL type 62 on the Argus; our Argus PT Ultra
  capture shows none.

## 4. Scope

### 4.1 In scope (v1)

**Server and protocol**

| ID | Requirement |
|---|---|
| R1 | Pure-Rust RTSP 1.0 server (RFC 2326) behind cargo feature `native-rtsp`. Methods: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE (200; stop sending, keep the session), TEARDOWN, GET_PARAMETER and SET_PARAMETER (200, empty body). RECORD and ANNOUNCE get 405 with an `Allow` header. `Public:` lists GET_PARAMETER. |
| R4 | Transport: RTP/AVP/TCP interleaved only. For a SETUP offering several transports, pick the first acceptable one and echo exactly one. If none is TCP, answer `461 Unsupported Transport`; ffmpeg and live555 then retry over TCP. UDP and multicast are v1.x (§4.2). |
| R18 | Wire profile: SDP and header shapes MUST match golden transcripts recorded from the current GStreamer server, except for the deltas listed in §5. Includes `Content-Base` with a trailing slash; `a=control` URLs that resolve both relative and absolute; `a=range:npt=now-` or `0-`; `a=framerate`; `RTP-Info` with `url`, `seq` equal to each track's first packet, and `rtptime` the RTP time of the play start (the first video packet's wire time) on every track. That equals the video's first packet; audio starts at or after it, which is the RFC 2326 mapping; and the `Session` header with `;timeout=`. |
| R8 | Mounts: the 19 aliases per camera, as today. Main: `/{name}/main`, `/Main`, `/mainStream`, `/MainStream`, `/Mainstream`, `/mainstream`. Sub: `/sub`, `/Sub`, `/subStream`, `/SubStream`, `/Substream`, `/substream`. Extern: `/extern`, `/Extern`, `/externStream`, `/ExternStream`, `/Externstream`, `/externstream`. Plus `/{name}`, which resolves to the first **configured** stream of main, sub, extern (today's rule). Matching is exact and case-sensitive after percent-decoding; a trailing slash and the query string are ignored, and so are track suffixes (`/trackID=N`, `/stream=N`). |

**Media**

| ID | Requirement |
|---|---|
| R2 | Video: H.264 (RFC 6184: single NAL and FU-A) and H.265 (RFC 7798: single NAL and FU, never AP) from camera Annex-B, with no transcoding. A NAL whitelist MUST be applied (H.264 types 1–13; H.265 types 0–9, 16–21, 32–40 with `nuh_layer_id = 0`; never a NAL with `forbidden_zero_bit` set or, for H.265, `nuh_temporal_id_plus1 = 0`). Filtered types are logged once per stream. RTP **packet** size (12-byte header + payload) ≤ `rtp_mtu` (default 1400). |
| R3 | Audio: AAC passthrough as RFC 3640 `mpeg4-generic` AAC-hbr (`sizelength=13; indexlength=3; indexdeltalength=3; config=` derived from ADTS). An ADTS frame with more than one raw data block is split when it has a CRC (using `raw_data_block_position`) and dropped, with a logged warning, when it has none (the Argus always sends one block). `audio = "aac" \| "none"`, default `aac`. ADPCM: the track is dropped with a warning in v1 (§5). |
| R10 | Parameter sets: always carried in SDP (`sprop-parameter-sets`, `profile-level-id`, `packetization-mode=1`; `sprop-vps/sps/pps`). In-band handling is `parameter_sets = "strip" \| "passthrough" \| "inject"`: strip removes them in-band (bairelay's fix for HA re-publish → go2rtc exit 183); passthrough forwards the camera's copies; inject puts them as single-NAL packets at the IRAP's timestamp before every IRAP. Default `strip`, confirmed by A4b at Gate 3 (2026-10-11): go2rtc 1.9.14 fails `frame.jpeg` (ffmpeg exit 183) on the H.265 re-publish under passthrough and inject. ffmpeg's RTSP muxer aggregates the in-band VPS/SPS/PPS into one AP that go2rtc doesn't unpack, and an ffmpeg → go2rtc chain with no neolink in it fails the same way. Under strip it passes. Marker bit only on the last packet of an access unit. |
| R19 | Media timeline: exactly one per (camera, stream kind), in u64 µs. **Video** time is the camera's u32 µs counter, unwrapped. A **discontinuity** is a delta < 0 or > 2 s, or a camera stream restart; it re-latches the wire offset (R5) and never steps wire time backwards. **Audio** has no camera timestamp: it is kept in samples, anchored to video at first arrival, and advanced by sample count. The A/V offset is estimated as the median over a 20 s window of the arrival-time offset. When `\|median\| > 80 ms`, it is corrected by dropping or skipping one AU per second until it falls below 20 ms. An initial offset > 500 ms is re-anchored once, before any audio is sent. RTP timestamp = wire time × clock rate + per-session random offset (RFC 3550 §5.1). |
| R5 | RTCP Sender Reports per track per session: at PLAY, then every 2 s. NTP and RTP time are both pure functions of one monotonic per-Hub **wire clock** shared by all tracks. The SR slope therefore always equals the RTP slope, and any timestamp re-latch moves NTP and RTP together on every track. `rtcp_sr = false` replaces SRs with an empty-RR keepalive every 2 s, so a client always receives something within 2 s. RTCP RR is accepted as liveness. |
| R20 | Pacing: one shared pacer per camera stream (not per client) schedules frame **release** by media time with a buffer of `pacing_latency_ms` (range 0–3000; default 1500 for battery cameras and 300 otherwise). Pacing never changes timestamps. The first frame after a start is released immediately. Excess latency is recovered using the minimum lead over a 10 s window. |

**Clients, fan-out and pause**

| ID | Requirement |
|---|---|
| R9 | One camera stream per (camera, stream kind), fanned out to N clients. Frames are shared by reference counting. |
| R12 | Late joiners: send the cached last IRAP access unit, re-stamped to now, then live frames from the next IRAP. Audio for that client starts at media time ≥ that IRAP. |
| R21 | Bridging: when no live video has been released for 3 s + pacing latency (`pause.on_motion` idle, outage, wake-up), send the cached IRAP at `bridge_fps` (default 0.2). Each bridged IRAP is followed by an end-of-sequence NAL, and timestamps track wall time. Silent AAC AUs are sent on the audio track while live audio is dropped. SRs continue. This implements the documented but unimplemented `pause.mode = "none"`. Wire timestamps stay monotonic per track across bridge entry and exit. Outage bridging is capped at `bridge_max_s` (default 120), after which sessions get BYE and close; motion-pause bridging is uncapped. The last IRAP is persisted (`persist_irap`), so bridging works from a cold start. `use_splash = false` disables bridging. |
| R11 | DESCRIBE MUST be answered within 2 s whatever the camera state. The SDP comes from the live stream, else from a persisted per-stream cache. On a miss, a bounded **learning start** runs: mains cameras always; battery cameras at most once per mount per 10 min and once per client IP per 5 min. If no desc is ready within 2 s, the answer is `503` with `Retry-After: 10`. **A DESCRIBE answered from the cache MUST NOT start the camera stream**: only PLAY creates demand, so health probes don't wake battery cameras. The SDP is frozen per session. A change in codec, resolution or parameter sets closes old sessions (BYE). Audio appearing late, or a framerate change, does not close sessions. |
| R13 | Bounded per-client queue, measured in **bytes**: budget = max(4 MiB, 2 × the largest GOP seen), which covers ≥ 1 GOP + pacing latency at the camera's bitrate. On overflow: skip to the next IRAP. Three overflows within 60 s drop the client (logged). A slow client MUST NOT delay the camera stream or other clients. |
| R22 | `pause.on_motion` keeps working: no camera stream without motion, except for the bounded learning start of R11. Clients are bridged (R21) while there is no motion. In the native backend, the 5 s stream-learning permit is replaced by the cache plus the learning start; the GStreamer backend keeps it. |

**Security**

| ID | Requirement |
|---|---|
| R6 | Auth semantics are unchanged: `users`, `permitted_users`, `anyone`, `anonymous`, and the default of anonymous access when no users are configured (logged as a warning). Digest (MD5, `qop=auth`, and always also RFC 2069 no-qop responses) is challenged **first**, Basic second; `rtsp_auth = ["digest","basic"]` configures this. Basic over plaintext logs a startup warning. Digest validates against the header's `uri=` parameter. Nonces are bound to the client IP, with a 5 min TTL; expiry sends `stale=true`. A user with no password can authenticate only with an empty password. An authenticated user who is not in `permitted_users` gets **404**, which matches the golden transcript. `anyone` means "every configured user", as today. Users and permissions hot-reload, and changes apply to new requests at once. |
| R23 | Sessions: session ID ≥ 64 random bits (CSPRNG), bound to its TCP connection and to the authenticated user; every request is re-authorized. Interleaved channels are validated (pairs allocated by SETUP, no duplicates; data on unknown channels is dropped). |
| R24 | Limits (configurable): max 128 connections total. **Unauthenticated** connections (neither credentials accepted nor a session set up) are at most 32 per IP (IPv6: per /64) and 64 in all; past either, an old one is closed rather than the new one refused: the IP's own oldest, or overall the oldest of the IP holding the most, so one IP (or a few) can't lock others out. Authenticated connections and connections with a session (also anonymous ones) are limited only globally, because Frigate, go2rtc and HA share one IP. An unauthenticated connection is closed 30 s after it opened (or lost its session), with or without users configured. First request within 10 s; idle connection without a playing session closed after 60 s; 8 KiB request head, bodies ≤ 4 KiB; 20 requests/s per connection. Failed auth: 10/min per (IP, claimed username) and 50/min per IP across usernames, where only requests that *carried* wrong credentials count; past either, credentials for that username (past the per-IP ceiling, any username) from that IP are answered at once as wrong ones would be (401, or anonymous on an anonymous mount; also instead of R6's 404 or `stale=true`), except on a connection that already authenticated as that user (its session keeps working); nothing is delayed. The snapshot endpoint (P5) applies the same failed-auth limits to its Basic auth, with its own counters. Out of file descriptors, accept retries every 100 ms. Malformed input never panics. |
| R25 | Credentials are never logged at any level: passwords, `Authorization` headers, `user:pass@` URIs. `CameraConfig` and `UserConfig` get redacting `Debug` (today `camthread.rs:40` trace-logs passwords). |

**Build, packaging and operations**

| ID | Requirement |
|---|---|
| R26 | Backend selection: subcommands `rtsp` and `mqtt-rtsp` exist whenever any RTSP backend is compiled in. Runtime key `rtsp_backend = "native" \| "gstreamer"` (CLI `--rtsp-backend`); choosing one not compiled in is a startup error. Default: the only compiled-in backend; when both are compiled in, `gstreamer` during beta and `native` after the §8 flip. Release CI builds use explicit feature lists, never `--all-features`. Running with no subcommand means `rtsp` whenever an RTSP backend exists. |
| R15 | Without `gstreamer`, `image` (SNAP path) and `stream_while_live`/`pause` logic are un-gated. `image --use-stream` and `talk` stay as subcommands and exit 2 with "requires a build with the gstreamer feature". |
| R7 | TLS is v1.x. In v1 the native backend refuses to start if `certificate` is set: "RTSPS is not yet supported by the native backend; use `rtsp_backend = "gstreamer"` or remove `certificate`". |
| R14 | Every existing config file parses. Keys with no effect in the native backend (§5.2) produce one warning each. |
| R27 | Graceful shutdown on SIGTERM/SIGINT (Ctrl-Break on Windows): send RTCP BYE, close sessions, stop camera streams, log out, exit 0 within 5 s. |
| R16 | Artifacts: static musl binaries for x86_64 and aarch64 Linux, plus a Docker image `FROM scratch` with CA certs and a `/state` volume. CMD is exec form and keeps `/usr/local/bin/neolink`. `NEO_LINK_MODE` is honoured by argv pre-processing in the binary (clap `env` can't choose a subcommand), so existing compose files work. The image runs as **root by default** for compatibility with root-owned 0600 configs; `PUID`/`PGID` drop privileges after the config is read. A `:native-debug` variant is busybox-based and keeps an `/entrypoint.sh` shim. `JEMALLOC_SYS_WITH_LG_PAGE=16` is kept for aarch64. `strip = "symbols"`. armv7, macOS, Windows and i686 are v1.x; i686 is dropped (release notes). |
| R17 | Release builds have no OpenSSL and no protoc: `pushnoti` (already non-functional upstream, README) is excluded from release builds. `build.rs` GStreamer/openssl link paths are gated on `CARGO_FEATURE_GSTREAMER`. |
| R28 | Observability: info-level events for client connect/disconnect (IP, user, path), auth failure (IP, user), camera stream start/stop/rebase/bridging, slow-client drop. Every 60 s at debug level, and on `SIGUSR1` at info level: a per-stream stats line (clients, bitrate, fps, drops). |
| R29 | Trust boundary documented: the MQTT `config` topic can rewrite users and permissions (but not power settings, P14). New key `mqtt_config_writes` (default `true` for compatibility, with a startup warning when users are configured). |

### 4.1.1 Power management (first-class; owner use case: on-demand viewing)

**Use case (owner decision, 2026-10-10):** a battery/solar camera that is viewed on demand
(HA dashboard, VLC). It is not recorded 24/7, and motion-triggered recording is not a v1
goal. The camera must be asleep whenever nobody is watching.

**Measured or verified problems in today's neolink**

- `idle_disconnect` defaults to `false`, so neolink stays logged in and pings every 5 s
  forever (`config.rs:218`, `camthread.rs:64`).
- It connects at startup unconditionally (`neocam.rs:63`).
- Even with `idle_disconnect = true`, the startup connection is never dropped until a first
  use cycle has finished (`neocam.rs:357-366`).
- An unreachable camera is retried every ≤ 5 s indefinitely (`camthread.rs:108`).
- MQTT polls snapshots and battery on timers (`mqtt/mod.rs:480-540`).

| ID | Requirement |
|---|---|
| P1 | **Sleep by default.** A camera whose `battery` is true or unknown is not connected unless something needs it right now: a playing RTSP session, a CLI or MQTT command, or a learning start. This holds at startup too, so neolink makes **no** connection at boot. Always-on requires `idle_disconnect = false` set explicitly in the config (presence detected per R14), and it logs a warning: "keeps <camera> awake; expect battery drain". |
| P2 | **Release quickly.** After the last use, the camera is disconnected (logout, socket closed) within `idle_grace_s`, default **10 s** (today 30 s). The grace covers a client that reconnects at once (DESCRIBE→PLAY, or an ffprobe followed by ffmpeg). |
| P3 | **Zero contact while idle.** While disconnected, neolink sends nothing to the camera: no pings, no stream-info, battery, snapshot or time polls, no discovery. Periodic tasks run only while the camera is already connected for another reason, and never cause a connection. Values they would fetch are served from cache, with their age. |
| P4 | **Bounded reconnect.** When a connection attempt fails while there is demand, retry with backoff (1, 2, 4… 60 s). With no demand, don't retry at all. A camera that is offline for 10 min with demand moves to a slow retry (5 min) and logs it. |
| P5 | **Cached snapshot endpoint.** An HTTP `GET /{camera}/snapshot.jpg` on a configurable port (`snapshot_port`, default `bind_port + 1`) serves the camera's own JPEG (`get_snapshot`), captured during the last wake (at the end of each streaming session, and at most once per wake), with `Last-Modified`. It **never** wakes the camera by default. `?fresh=1` may wake it; this is rate-limited and configurable, and off for battery cameras by default. This lets HA's camera card refresh thumbnails without waking the camera. It uses the same auth as RTSP (Basic and Digest). |
| P6 | **Wake ledger.** Every connection is recorded with a reason (`rtsp:<peer>:<path>`, `cli:<cmd>`, `mqtt:<topic>`, `learn`, `startup-always-on`), the start time, awake duration and bytes streamed. A per-camera summary line is logged at info on each disconnect (`argus: awake 23.4 s for rtsp:192.168.86.10:/argus/sub; today: 3 wakes, 71 s awake`). The ledger is published on MQTT if it is enabled (retained JSON on `neolink/<camera>/status/wakes`), and more than 12 wakes in an hour logs a warning naming the reasons. This is how power use is observed and tested. |
| P7 | **Battery reads only while awake.** `battery` CLI/MQTT values come from the last reading taken during a wake (each wake reads it once, piggybacked). Only an explicit request may wake the camera for it: CLI `battery --fresh` / `image --fresh`, or an MQTT query with the payload `fresh` (concurrent `fresh` queries share one wake; a reading at most 60 s old is answered without waking). |
| P9 | **Wake path.** Measured on fw v3.0.0.5808 (2026-10-10, 20:22, camera asleep, TCP 9000 refusing): **local UDP discovery reaches and wakes the sleeping camera**, and discovery → login → battery read took 7 s. bairelay's claim that only the cloud can wake it does not hold for this camera. For battery cameras, discovery runs **local first; cloud registration (`C2R_C`) only if local fails *and* `discovery` allows it; never both in parallel.** That is a change to `find_camera`'s `select!`. Every cloud attempt counts as a wake attempt in the ledger, and P4's backoff applies to it. |
| P10 | **No permit without a reason.** Boot-time tasks that take permits are removed or turned into wake piggybacks: the version/stream-info report (`neocam.rs:197-229`), the UID fetch (`:231-249`), and push registration (`:252-302`, pushnoti only). The idle loop observes the use-counter with a subscription, not an activated permit (it races today, `neocam.rs:359-360`). Only callers that pass a P6 reason can create permits. |
| P11 | **Deadlines.** Every `run_task` has a deadline (default 60 s) and then fails. "Demand" for P4 retries means a playing RTSP session or an unexpired command, never a stuck task. Duplicate MQTT queries are coalesced. |
| P12 | **Motion permits.** The motion-permit thread (`neocam.rs:304-330`) runs only when `pause.on_motion` (or another explicit motion consumer) is configured. `MdState` resets to `Unknown` on disconnect. A motion permit is held for at most `motion_hold_max_s` (default 120). Its ledger reason is `motion`. |
| P13 | **Clean release.** Every disconnect path logs out (2 s timeout) and, over UDP, sends `C2D_DISC`. Today an idle disconnect drops the future without logging out (`camthread.rs:126-137`), and `C2D_DISC` is never sent. The fixed 4 s of sleeps after login (`camthread.rs:44,48`) become retry-on-400, so a wake is shorter. The TCP probe login as `admin`/`123456` (`discovery.rs:1079-1111`) is removed. |
| P14 | **MQTT does not undo power settings.** Power settings come only from the config file: a config written to the MQTT `config` topic that changes any camera's `battery`, `idle_disconnect` (any alias), `idle_grace_s`, `motion_hold_max_s`, `snapshot_fresh` or `pause.on_motion` from the file's value (omitting a key the file sets is a change), or enables a camera the file disables, is refused whole with a `FAIL:` reply on `config/status` naming the cameras and keys, and a warning per camera. A camera the file doesn't have takes the defaults for those keys (it sleeps). It applies whenever `mqtt_config_writes` allows writes at all. Config is pushed per camera only when that camera's config changed, and only connection-affecting keys reconnect (`reactor.rs:104-107` pushes to every camera today). Retained `config` messages from before process start are ignored. Retained `control/*` and `query/*` messages are ignored with a warning. `idle_disconnect` and `battery` are `Option` and skipped when unset, so a config dump never turns into an "explicit always-on". HA availability comes from the neolink process's last-will, plus a separate `awake` binary sensor, so a sleeping camera isn't shown as "unavailable". While a battery camera is awake: one snapshot and one battery read per wake, with the polling timers suppressed. |
| P15 | **One state file per camera** in `{state_dir}/camera/<hash>.json`, shared by every mode and process. It holds battery (yes/no/unknown), supported streams, UID, model and firmware, the last battery reading, and the last snapshot. Battery is true once `battery_info` succeeds; it is never inferred as mains from `adapterStatus` or `chargeStatus` (solar reports "solarPanel"). A lock file warns when a second neolink process uses the same camera. CLI `battery` and `image` read the state file by default and wake only with `--fresh`. |
| P16 | **GStreamer backend under P1.** Mounts come from the state file's supported streams (or config), so they exist before any wake. Documented: the GStreamer backend still wakes the camera on DESCRIBE (R11's protection is native-only). |
| P17 | **One retry owner** (P4). The Hub's linger is folded into `idle_grace_s`. Awake-time budget per on-demand session: session length + `idle_grace_s` + login and logout overhead (≤ 5 s). |
| P18 | **Learning starts** are limited **per camera** (one wake teaches every stream kind), with the limiter timestamp persisted in the state file. |
| P19 | **Documented HA recipe for battery cameras:**<ul><li>still image URL = the P5 endpoint;</li><li>"Preload stream" off;</li><li>`camera_view: auto` (not `live`);</li><li>no Frigate or go2rtc `preload:` for this camera;</li><li>no `command_line` polling;</li><li>no ping or nmap device tracker on the camera's IP.</li></ul>P5 returns a placeholder JPEG (HTTP 200) when the cache is empty, so HA's config flow accepts the URL. |
| P8 | **24/7 warning.** If a session on a battery camera lasts more than 30 min, log once: "<camera> has streamed continuously for 30 min; battery cameras are not designed for continuous recording". It is informational and never enforced. |

**Out of scope (v1):** waking on motion without staying connected. That needs a local
push/wake receiver, as bairelay has; it is a v1.x candidate. `pause.on_motion` on a battery
camera keeps today's behaviour of staying connected, and P1's warning applies to it.
*As implemented (PLAN Gate 5):* `pause.on_motion` does **not** keep a sleeping camera
connected. The camera still sleeps, motion can't start its stream, and neolink warns at
startup; staying connected needs `idle_disconnect = false` (or `battery = false`).

### 4.2 Deferred to v1.x (in order)

1. UDP unicast: server port range config, RTP sent only to the RTSP peer IP, `destination=` rejected, RTCP accepted only from the negotiated peer and ports, per-IP caps, paced sends.
2. RTSPS via rustls+ring (`certificate`, `tls_client_auth`, new `tls_client_ca`, hot reload; UDP refused under TLS).
3. ADPCM → PCMU, and `audio = "l16" | "pcmu"` via a pure-Rust AAC decoder (symphonia).
4. armv7 musl, macOS arm64, Windows x86_64 artifacts.
5. Metrics endpoint, `check-config`, unknown-key warnings.

### 4.3 Out of scope

Multicast, RECORD, RTSP 2.0, SRTP, RTSP-over-HTTP tunnelling, video transcoding, AAC
encoding, replacing GStreamer for `talk` and `image --use-stream`, Unifi Protect (it needs ONVIF).

## 5. Deliberate behaviour changes vs the GStreamer backend

### 5.1 Changes on the wire (each needs a release-notes entry)

| Area | GStreamer backend | Native backend | Why |
|---|---|---|---|
| Camera streams for N clients | N | 1 per stream kind | Battery life; camera stream limits |
| AAC | Decoded to L16. faad upmixes the Argus's mono to stereo (measured). | AAC passthrough | No C decoder; XProtect can't play L16 (neolink #302) |
| ADPCM | Probably broken (`layout="div"` typo, unverified) | Track dropped with a warning (v1); PCMU in v1.x | Scope |
| Missing-audio silence fill | `fallbackswitch` inserts silence after 3 s (when the plugin exists) | None; the audio track simply pauses | Scope; revisit if a client needs it |
| Video timestamps | Synthetic `n·1e6/fps` from 0, u32 (wraps at 71.6 min); fps from Info frames, which the Argus PT Ultra does not send (measured) | Camera clock, unwrapped and rebased (R19) | Drift, wrap (#398) |
| Pacing | Implicit, via GStreamer's synced sinks against synthetic PTS | Explicit shared pacer (R20) | Equivalent intent |
| Splash | MJPEG test pattern; EOS after 500 frames forces a reconnect | Removed: cached SDP + bridging, else 503 | A splash SDP can't turn into H.265 within a session |
| Startup | DESCRIBE blocks until the camera stream is learned (unbounded) | ≤ 2 s (R11) | go2rtc's 5 s request deadline |
| UDP / RTSPS | Supported | 461 / startup error (v1) | Scope and reflection risk; v1.x |
| Auth | Basic only | Digest first, then Basic | Compatibility, security |
| Session timeout | `timeout=30` | `timeout=60`, bound to the TCP connection; keepalives accepted, not required | TCP only in v1 |
| SDP | `s=Session streamed with GStreamer`, `a=tool:GStreamer`, `a=ts-refclk`, `a=mediaclk`, `a=ssrc`, PT 96 on both m-lines | `s=`/`a=tool` say neolink; refclk/mediaclk/ssrc attributes dropped (SSRC stays in Transport); `a=framerate` added; PT 96 kept on both | Honest identification; fps for clients |
| Default transport for plain ffmpeg | UDP (measured) | 461, then TCP fallback (A2b) | v1 is TCP-only |
| `Public` header | Also lists ANNOUNCE and RECORD | Play methods only (RECORD/ANNOUNCE get 405) | Honest capabilities |
| Stream the camera doesn't have (e.g. the Argus's extern) | Mount exists; DESCRIBE answers 400 | No mount: 404 | Correct status |
| After TEARDOWN | `Connection: close` and close, no BYE | Same; BYE only when the server ends a session (e.g. A18) | Matches GStreamer |
| A/V start offset | Audio starts about 1.9 s before video (measured with ffprobe start times) | One wire clock (R5); A16 ≤ 150 ms | Bug fix |

### 5.2 Config keys

- **Still meaningful:** `bind`, `bind_port`, `users`, `permitted_users`, `stream`, `enabled`, `channel_id`, `strict`, `pause.on_motion`, `idle_disconnect`, and all camera connection keys.
- **Ignored with a warning in native:** `splash_pattern`, `use_splash` (meaning becomes "bridge"), `certificate` (error, see R7), `tls_client_auth`.
- **Already dead in both backends (warn):** `buffer_duration`, `print_format`, `tokio_console`, `pause.mode`, `pause.on_disconnect`/`on_client`, `pause.motion_timeout`.
- **New:** `rtsp_backend`, `rtsp_auth`, `rtp_mtu`, `pacing_latency_ms`, `parameter_sets`, `rtcp_sr`, `audio`, `state_dir`, `bridge_max_s`, `bridge_fps`, `persist_irap`, `battery`, `mqtt_config_writes`, limits (R24).

## 6. Non-functional requirements

Each NFR names its measurement method; numbers are targets to confirm in PLAN phase 0.

| ID | Requirement | Method |
|---|---|---|
| N1 | DESCRIBE → first video RTP ≤ 2 s (camera streaming), ≤ 2 s plus wake time (camera asleep) | Log timestamps in the neolink server plus a retina test client |
| N2 | Added latency excluding `pacing_latency_ms`: p99 ≤ 50 ms | Ingest→write histogram from the `metrics` cargo feature, from the fixture replay (log level doesn't matter: `release_max_level_debug` compiles out trace) |
| N3 | One 4K H.265 15 fps stream (2.4 Mb/s, the Argus rate) to 4 TCP clients: ≤ 5 % of one core of the CI runner (ubuntu-24.04 x86-64) from fixture replay; the Baichuan decryption cost is measured once on hardware and added. No Pi 4 is available (owner, 2026-10-10); an aarch64 QEMU run is informative only | `pidstat -p` over 120 s, info log level |
| N4 | Process RSS growth per added client ≤ 1 MiB (frames shared, R9) | RSS with 1 vs 5 clients after 60 s |
| N5 | 24 h soak with 2 clients: RSS growth ≤ 10 % after the first hour; no client reconnect; no on-wire timestamp jump > 1 s outside logged rebases. **Run on fixtures** (looped real capture plus the synthetic wrap/outage/congestion variants), because there is no mains-powered camera (owner, 2026-10-10); a hardware soak is deferred until one exists | ffmpeg `-c copy` logs + neolink logs |
| N7 | Fuzzing: RTSP parser, Transport/Session/Authorization parsers, Annex-B splitter, ADTS parser. Smoke run of 60 s per target per PR; 30 min per target nightly | cargo-fuzz |
| N9 | `podman stop` returns in < 5 s with exit code 0 | CI container test |

## 7. Acceptance tests

Where each runs: **F** = CI with fixture replay, **B** = CI build check, **H** = real
hardware, **M** = manual. Fixtures for CI are synthetic: ffmpeg-generated H.264/H.265/AAC
wrapped in BcMedia framing, with timing that imitates the measured Argus bursts. Captures of
the owner's camera are not committed (privacy) unless the owner explicitly allows it.

| ID | Where | Test | Pass criteria |
|---|---|---|---|
| A1 | F, H | `ffprobe -rtsp_transport tcp`, main and sub | Codec and resolution exact; fps ±5 %; audio track present |
| A2 | F, H | `ffmpeg -t 120 -c copy`, then decode with `-v error` | 0 error lines; no `RTP: missed`/`PPS id out of range` lines |
| A2b | F | Plain `ffmpeg`, no `-rtsp_transport` (tries UDP first) | Falls back to TCP after 461 and passes A2 |
| A3 | F, H | Frigate `preset-rtsp-generic` and `preset-rtsp-restream` ffmpeg command lines. CI: 15 min with the fixture starting at cam_us = 2³²−5 min, so it crosses the wrap. Nightly: 90 min. Hardware: 10 min | 0 reconnects in the client log |
| A4 | F, H | go2rtc pulls `rtsp://`, then `frame.jpeg` | JPEG returned |
| A4b | F | neolink → `ffmpeg -c copy -f rtsp` → go2rtc → `frame.jpeg` (the HA re-publish path) | JPEG returned; decides the `parameter_sets` default |
| A5 | F, H | `cvlc` for 300 s over TCP, default caching. It is stopped through its rc interface: `--run-time` makes VLC 3 quit early after RTCP sync | Plays; < 1 "picture is too late" per minute |
| A6 | F | retina client (TCP; Digest and Basic) | Depacketized NALs byte-identical to the input after the R2 filter; AAC frames identical |
| A7 | F, H | 4 clients join at staggered times, including one during camera start-up; plus main+sub concurrently. Run with `parameter_sets = strip` and with `inject` | 1 camera stream start per kind; each client's first video frame is an IRAP and decodes (from SDP parameter sets alone under `strip`) |
| A8 | F | 100 connect/teardown cycles, run both against the fixture source in `crates/rtsp` and against the real `source.rs` with a mock `NeoInstance` | Camera stream stops ≤ 5 s after the last client; `num_alive_tasks` and the camera use-counter return to baseline |
| A21 | F | Health probe: a DESCRIBE every 10 s for 10 min against a cached battery mount | 0 camera stream starts |
| A23 | F | Idle power: scripted peer marked battery; neolink **boots** and runs 1 h (virtual time) with no clients, MQTT enabled with default intervals, a DESCRIBE probe every 10 s, broker reconnects every 5 min, retained `control/wakeup` and `query/battery` present, and an unchanged config re-published every 10 min | **0** connections, **0** discovery broadcasts, **0** cloud registrations (mock p2p host) (P1, P3, P9, P10, P14, R11) |
| A28 | F | Stuck work: the camera is unreachable and an MQTT query with the payload `fresh` arrives (`tests/e2e/power.sh` uses `query/pir fresh`); a motion Start with no Stop, then disconnect | The query fails at its deadline; there are no retries after it; the motion permit is released on disconnect or at `motion_hold_max_s` (P11, P12) |
| A29 | F | Multiple processes: an `rtsp` and an `mqtt` instance on the same camera, plus CLI `battery` | The second process warns; CLI `battery` makes 0 connections (P15) |
| A30 | F | GStreamer backend with P1: on-demand PLAY works with an empty and with a warm state file | Mounts exist before any wake; the camera wakes on demand (P16) |
| A24 | F | On-demand cycle: 20 PLAY sessions of 30 s each, spaced 2 min apart | 20 wakes; each awake time ≤ session + `idle_grace_s` + 2 s; ledger reasons correct (P2, P6) |
| A25 | F | Unreachable camera: no demand for 1 h, then demand for 10 min | 0 attempts without demand; backoff per P4 with demand |
| A26 | F | Snapshot endpoint: 360 `GET snapshot.jpg` over 1 h with no clients | 0 wakes; JPEG served from the last wake with a correct `Last-Modified` (P5) |
| A27 | H | Sleep check: one 1-minute viewing session, then 2 h idle. The method must not wake the camera: the neolink ledger and per-wake battery readings (voltage, current, %), plus `ss -u`/`ss -t` showing no neolink socket to the camera. Do **not** open the Reolink app, which wakes the camera; do not ping continuously. Passive capture at the AP is used if the router allows it (Google Wifi doesn't, so this is likely unavailable) | Ledger: exactly 1 wake; no socket to the camera from `idle_grace_s` + 5 s after the session; the next session's first battery reading is consistent with sleep (no unexplained drop) |
| A22 | F | Wire monotonicity under random bridge/resume/discontinuity/pause schedules (property test) | RTP timestamps per track never decrease; SR NTP−RTP mapping identical across tracks |
| A9 | F | A client that stops reading (SO_RCVBUF 4 KiB) | Other clients' frame timing unaffected (inter-frame p99 delta < 50 ms); slow client dropped per R13 |
| A10 | F | Auth matrix × clients (ffmpeg, go2rtc, VLC, retina): none / wrong / Basic / Digest qop / Digest no-qop / user not permitted / `anyone` with valid creds / `anyone` without creds | Matches the golden transcript per row |
| A11 | F, H | Camera outage of 60 s mid-stream. Fixture: stall the feed. Hardware: an nftables drop of the camera's IP on the neolink host | ffmpeg, go2rtc and VLC keep their sessions; live media resumes from an IRAP; per-session RTP timestamps monotonic; no on-wire jump > 1 s |
| A12 | F, M | `pause.on_motion`: 60 s idle, then motion. A **10 min idle** variant checks that motion pause is not capped by `bridge_max_s`. Motion in F is injected `MdState`; on hardware it is a person in front of the PIR (owner in the loop) | No camera stream while idle (except one learning start on an empty cache); sessions survive both variants; first live IRAP ≤ motion event + 2 RTT + 1 GOP; Frigate tolerates `bridge_fps = 0.2` |
| A13 | B | Static binary + scratch image | `file` reports a static binary; container starts; N9 |
| A14 | — | ~~Blue Iris~~: dropped (owner decision 2026-10-10) | — |
| A15 | H | 24 h soak | N5 |
| A16 | F | A/V sync, using a fixture with a periodic flash plus beep | \|A−V\| ≤ 150 ms at 5 s, 60 s and 15 min in CI; at 90 min nightly |
| A17 | F | u32 wrap (fixture starting at 2³²−5 s) and counter reset | No on-wire jump > 1 s; retina accepts the timeline |
| A18 | F | Mid-stream resolution or codec change | BYE plus close; the client's re-DESCRIBE gets the new SDP |
| A19 | F | Golden transcript diff vs the GStreamer backend, all aliases | Only the §5 deltas differ |
| A20 | F | Security: header > 8 KiB, slowloris, 100 unauthenticated connections from one IP, 24 simultaneous authenticated DESCRIBEs from one IP (must all succeed), session ID used from another connection, Digest nonce replay after 5 min, credentials in logs (grep) | Rejected, limited or accepted per R23–R25; no credentials found in logs |

Also tested where available: Scrypted, Synology Surveillance Station, Milestone XProtect,
Android Media3.

## 8. Rollout

1. **v1-beta:** `native-rtsp` is opt-in and published as a `:native` Docker tag; the GStreamer image remains `:latest`.
2. **Default flip:** when every F and B test in §7 passes in CI (including A21 and A22), every H test passes on the Argus, a 2-week soak passes, and there are no open P1 compatibility bugs for Frigate, go2rtc, VLC or Blue Iris. Then `:latest` becomes native and `:gstreamer` keeps the old image.
3. **Deprecation:** the `gstreamer` backend stays a non-default build for at least 2 releases.

A static musl binary cannot contain GStreamer, so "fallback" means a different artifact or
tag, not a runtime switch inside the static build.

## 9. Measured facts and open questions

Measured on the Argus PT Ultra, fw v3.0.0.5808_26041006: a 20 s main+sub capture (2026-10-10) and a 570 s main capture followed by logout, 180 s idle and a 20 s reconnect (2026-10-11 08:31, morning, solar):

| Question | Answer |
|---|---|
| Proprietary NAL 62 / layer ≠ 0 | None seen. Main has VPS/SPS/PPS + 2 IDR_W_RADL slices per IRAP, then TRAIL_R; sub has SPS/PPS/IDR, then non-IDR. |
| GOP | 30 frames = 2 s, both streams |
| Video clock | Monotonic, 66,667 µs/frame (15.00 fps); main and sub share one clock; an occasional 200 ms gap (dropped frame) on sub |
| Info frames | Not sent |
| Audio | AAC-LC ADTS, 16 kHz, **mono**, 1 raw block, 64 ms/frame; no timestamp |
| Arrival jitter | 20 s capture: max gap 1.3–1.5 s (Wi-Fi bursts), p50 22 ms. 570 s capture: video inter-arrival p50 38 ms, p99 905 ms, max **3.1 s**; 64 gaps > 1 s, 11 > 1.5 s |
| Arrival lag | Steady state: the least-delayed frames arrive 0.93 s after their camera time. **Two congestion episodes** (at 40–80 s and 240–300 s) where the lag grew to **17 s** and 7 s: the camera buffers and delivers everything late rather than dropping, then catches up in a burst. 2 % of frames are dropped by the camera (133 ms steps). Main averages 2.37 Mb/s |
| Audio vs video (Q1) | `err` (DESIGN §4.2) median **+8 ms**, p5 −35, p95 +48, range −67…+91 ms; both signs occur. Drift **−1.2 ± 25.4 ppm** (95 %, start-up windows skipped, interval widened for autocorrelation; `analysis/drift.py`), i.e. not measurable over 10 min. Audio and video share the camera clock; `skew_estimate` defaults to 0 and the median estimator is confirmed |
| Camera vs host clock | −9.6 ± 25.8 ppm (95 %), from the lower envelope of the 23 steady 20 s windows (congestion excluded; `analysis/drift.py`): at most ~0.15 s over a full 71.6 min `cam_us` wrap |
| Frame sizes | Main IRAP up to 763 KB (about 550 packets), P-frames up to 359 KB; sub IRAP ≤ 139 KB |
| Concurrency | Main + sub run concurrently on one Baichuan connection |
| Wake path (20:22, camera asleep) | TCP 9000 refused while asleep **and stays closed while awake** (Baichuan runs over UDP on this model), so TCP 9000 is not a sleep indicator. Local UDP discovery woke the camera: 7 s from start to battery read. The camera's awake time after disconnect is **unmeasured** (open question 6). |
| Reconnect (Q2) | Discovery + login took 0.6 s when the camera was awake (08:31), **2.0 s** 180 s after a clean logout, 7 s from overnight sleep. `cam_us` **continues** across logout and reconnect (advanced 185.6 s over 186.3 s of wall time; the difference is arrival lag), and so does `cam_time`. The **first `battery_info` right after that wake returned 400**; P7's piggyback must retry, not give up |
| Streaming cost | 9.5 min of main: battery 55 → 54 %, 3.70 V, −0.47 to −0.52 A, `charge=none` in morning sun. The 180 s idle + 20 s reconnect cost < 1 % |
| Exit bug | `neolink battery` logs `Connection Lost: Thread panicked` on exit (fixed in PLAN 1.5) |

Golden transcripts from the GStreamer backend (2026-10-10, ffmpeg 8 client):

| Item | Observed |
|---|---|
| OPTIONS `Public` | OPTIONS, DESCRIBE, ANNOUNCE, GET_PARAMETER, PAUSE, PLAY, RECORD, SETUP, SET_PARAMETER, TEARDOWN |
| DESCRIBE | `Content-Base: rtsp://host:port/argus/main/` (trailing slash); `a=control:*` session-level; `a=control:stream=N` per media; `a=range:npt=0-`; PT 96 on both video and audio; L16/16000/2 audio |
| H.264 fmtp | `packetization-mode=1;sprop-parameter-sets=…;profile-level-id=640029;level-asymmetry-allowed=1` |
| SETUP reply | `Transport: RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=XXXXXXXX;mode="PLAY"`; `Session: <16 chars>;timeout=30` |
| PLAY reply | `RTP-Info: url=…/stream=0;seq=…;rtptime=…, url=…/stream=1;…`; `Range: npt=0.32-` |
| Auth | Basic only. No creds / wrong / unknown user → 401; valid user not permitted → **404**; unknown path → 404 |
| Re-record (2026-10-11, pinned ffmpeg n8.1.3, `crates/rtsp/tests/golden/gst`) | All 19 aliases recorded. A re-recording of `/argus/main` has **no structural deltas**. A stream the camera lacks (`/argus/extern`) gives **400** at DESCRIBE, not 404. One PLAY of `/argus/Main` got **503** when it followed the previous client's TEARDOWN immediately (a GStreamer media-reuse race; the next 17 opens succeeded). `neolink rtsp` exits within 4 s on SIGTERM |

Still open:

1. ~~Long-run drift between the audio sample clock and the camera video clock.~~ Not measurable over 10 min (−1.2 ± 25.4 ppm); see the table (2026-10-11).
2. ~~Camera µs counter behaviour across reconnect.~~ It continues across a clean logout and a 180 s idle (2026-10-11). Whether it survives a deep sleep (overnight) is unconfirmed; the MediaClock treats any jump as a discontinuity anyway (DESIGN §4.2), so this no longer blocks anything.
3. ~~Default for `parameter_sets` (A4b, Gate 3).~~ `strip` (2026-10-11): go2rtc 1.9.14 fails `frame.jpeg` (ffmpeg exit 183) on the H.265 re-publish under passthrough and inject. ffmpeg's RTSP muxer aggregates the in-band VPS/SPS/PPS into one AP that go2rtc doesn't unpack, and an ffmpeg → go2rtc chain with no neolink in it fails the same way. Under strip it passes.
4. Is the burst pattern the same on mains power, and on wired cameras? This decides the `pacing_latency_ms` default.
5. ~~Whether the owner allows a short capture of their camera as a CI fixture.~~ Yes (2026-10-10).
6. How long the camera stays awake after a clean logout plus `C2D_DISC`, compared with an abrupt drop. Partial data (2026-10-11): 180 s after a clean logout the wake took 2.0 s, between the awake (0.6 s) and deep-sleep (7 s) figures, so the camera was likely in a light sleep. This needs a non-waking observation method. Candidate: the battery current reported in the next wake's first reading, compared across sessions with different disconnect styles.
