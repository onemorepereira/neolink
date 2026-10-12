# Native RTSP: Design

Status: **v2.2** (2026-10-11). v2 was revised after two adversarial reviews (concurrency/timing,
traceability/integration). v2.1 applies the whole-doc-set review ("F-ds#"); v2.2 applies the Phase 0 measurements. Implements [SPEC.md](SPEC.md) v2.3. IDs (Rn, Nn, An) refer to
the spec; "F#" in margins refers to review findings this design answers.

## 1. Shape

```
                 neolink binary (src/)                          crates/rtsp (neolink_rtsp)
┌───────────────────────────────────────────────┐   ┌─────────────────────────────────────────────────┐
│ rtsp_native/                                  │   │ Server ── Listener (limits §6.6)                │
│  mod.rs     config → ServerConfig, mounts,    │   │   └─ Connection task (one per TCP conn)         │
│             supported-stream watch, shutdown  │   │        ├─ reader: RTSP + interleaved RTCP       │
│  source.rs  NeoInstance → StreamSource        │──▶│        ├─ Session (bound to conn + user)        │
│  auth.rs    users/permitted → AuthDb (watch)  │   │        └─ writer: chunked, prioritised          │
│ common/instance/stream.rs (un-gated,          │   │ Hub (camera × stream kind), one task:           │
│   cancellable, emits StreamEvent)             │   │   Ingest → MediaClock → Desc → Pacer →          │
│ common/reactor.rs  shutdown().await (new)     │   │   WireClock(+Bridge) → per-session queues       │
└───────────────────────────────────────────────┘   │ codec/ rtp/ rtsp/ sdp/ auth/                     │
                                                    └─────────────────────────────────────────────────┘
```

**Crate facts**
- `crates/rtsp` is independent of Baichuan and neolink. Its only input is `StreamSource`.
- It is a workspace member through the existing `members = ["crates/*"]`.
- Edition 2021, `rust-version = "1.82"`, and it logs with `log`.

**Dependencies, pinned for MSRV 1.82 (F-int 9)**
- `tokio` ≥ 1.39, so `RuntimeMetrics::num_alive_tasks` is stable. This bumps the workspace
  lockfile from 1.37.
- `bytes`, `md5`, `base64 0.22`, `rand 0.8`, `hmac 0.12`, `sha2 0.10`, `subtle 2.6`,
  `parking_lot` (no lock poisoning, F-cc 27), `arc-swap`, `tokio-util` (CancellationToken).
- Dev-dependencies: `retina =0.4.13` (the last release with MSRV ≤ 1.82) and
  `proptest ≤ 1.9`.

**Not used, and why**
- `rtsp-types`: the survey found its Session and Transport parsing too strict.
- `sdp-types` and `rtp-types`: the SDP is short enough to write by hand, and RTP headers are
  12 bytes.

## 2. Core types

```rust
pub enum VideoCodec { H264, H265 }

/// Decoder-relevant identity only (F-cc 4). Two descs are "the same stream" iff these match.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct VideoIdentity { codec: VideoCodec, params: ParamSets /* normalised NAL bodies */,
                           width: u32, height: u32 }
pub struct AudioDesc { asc: Bytes, sample_rate: u32, channels: u8 }   // AAC only in v1

pub struct StreamDesc {
    video: VideoIdentity,
    audio: Option<AudioDesc>,
    framerate: Option<u32>,   // informational, for a=framerate; NOT part of identity
    gen: u32,                 // bumped only when `video` changes or audio's ASC changes
}

pub struct Frame {
    track: Track,             // Video | Audio
    irap: bool,
    wire_ts: WireTs,          // set by the WireClock stage (§4.5), never by ingest
    rtp: Arc<[RtpChunk]>,     // packetized eagerly in the Hub (F-cc 23), shared by all sessions
    bytes: usize,             // payload size, for per-session byte budgets
    gen: u32,
}
/// One RTP packet minus its 12-byte header: a small prefix (FU / AU header, ≤ 4 bytes)
/// plus a zero-copy slice of the NAL or AU (F-cc 10).
pub struct RtpChunk { marker: bool, prefix: [u8; 4], prefix_len: u8, body: Bytes }
```

Restamping a frame (late joiner, bridge) produces a new `Frame` that shares the same
`Arc<[RtpChunk]>`. Only `wire_ts` differs, so media is never copied (F-int R12).

## 3. Source boundary and neolink side

### 3.1 Trait

```rust
pub trait StreamSource: Send + Sync + 'static {
    /// Start camera media. The source MUST stop and release every camera resource
    /// (permits, stream, motion watchers) promptly when `cancel` fires.
    /// `Learn` ignores `pause.on_motion` (bounded learning start, §4.3); `Demand` honours it.
    fn start(&self, mode: StartMode, cancel: CancellationToken)
        -> BoxFuture<'static, Result<mpsc::Receiver<SourceUnit>>>;
    /// Whether this stream should be served at all (camera reports it), and whether
    /// waking the camera is costly. Watched; changes apply live.
    fn availability(&self) -> watch::Receiver<Availability>;
}
pub enum StartMode { Demand, Learn }
/// `battery: None` (unknown) is treated as battery: fail safe (F-ds 8). Persisted in
/// desc.json, and the per-camera config key `battery` overrides it.
pub struct Availability { supported: Option<bool>, battery: Option<bool> }
pub enum SourceUnit {
    Video { codec: VideoCodec, irap: bool, cam_us: u32, annexb: Bytes },
    Aac { adts: Bytes },
    Restarted,               // the camera stream was re-established (not on first start)
    /// Why media is or is not flowing (F-ds 1). Paused = pause.on_motion idle (bridge is
    /// uncapped); Retrying = outage (bridge capped by bridge_max_s); Streaming = live.
    State(SourceState),
}
pub enum SourceState { Streaming, Paused, Retrying }
```

The Hub stops a source with `cancel.cancel()`, and only that way. A single mechanism
replaces v1's "drop the receiver / AbortHandle" ambiguity (F-int 5).

### 3.2 Neolink implementation (`src/common/instance/stream.rs`, un-gated)

1. **Cancellable `stream()` (F-int 1).**
   - The entire `run_task` call runs inside `select! { _ = cancel.cancelled() => …, r = run_task(…) => … }`.
     Dropping the `run_task` future drops its permit and `StreamData`, and that sends
     `MSG_ID_VIDEO_STOP` (`crates/core/src/bc_protocol/stream.rs:80-91`).
   - Waits that today never observe a dropped receiver (`camera_watch.wait_for` in
     `run_passive_task`, and `aquired_users()` in `stream_while_live`) are inside the
     select, so they are cancelled too.
   - The double `spawn` at `gst.rs:183-203` is replaced by one spawn whose `JoinHandle` is
     kept by `source.rs`.
   - New test `stream_cancel_releases_permit`: start, cancel, then assert that the camera
     use-counter returns to 0 within 100 ms and that no task is left alive.
   - **Test seam (F-ds 2).** `NeoInstance` and `BcCamera` are concrete, and `BcCamera::new`
     always does discovery and real I/O, so they can't simply be mocked. neolink_core
     therefore gets a `test-util` feature:
     - `BcCamera::from_parts(sink, source)` builds a camera over in-memory channels.
     - A **scripted Baichuan peer** answers login, keepalive, abilities and stream start,
       then replays `.bcm` media. It can also inject disconnects.
     - neolink tests build a real `NeoInstance` over it, and inject `MdState` changes
       through the existing motion watch.
     - This is the model bairelay uses (`MockConnection`/`FakeCameraBuilder`).
2. **`StreamEvent` (F-int 6).** The channel item becomes
   `enum StreamEvent { Media(BcMedia), Restarted }`.
   - `Restarted` is sent *after* `start_video` and the first successful `get_data`, on
     every closure entry except the first.
   - The 400-retry re-entry (`instance.rs:128-144`) counts as an entry.
   - `stream_while_live` starts a fresh `stream()` on each motion start, so it emits
     `State(Paused)` when motion stops, then `Restarted` + `State(Streaming)` when motion
     resumes.
   - `run_task` waiting on a camera reconnect emits `State(Retrying)`.
   - The GStreamer factory ignores the new variants (`rtsp/factory.rs:174,182,264`).
3. **`stream_while_live(kind, opts)` (F-int 4, F-cc 17).** `opts.init_permit: Option<Duration>`.
   - The GStreamer backend passes 5 s, which is today's behaviour.
   - Native passes `None`, and does a bounded learning start instead (§4.3).
   - `pause.on_motion` is re-read from the config watch on each start (F-int 18).
4. **Availability (F-int 8).** This ports the GStreamer backend's 15 s `get_stream_info`
   poll (`rtsp/mod.rs:260-291`) via `run_passive_task`, which does not wake the camera.
   - `battery` is `Some(true)` if `get_battery_info` succeeds once, and `Some(false)` if the
     camera reports no battery ability. Until then it is `None`, which is treated as battery.
   - Streams reported unsupported are answered with 404 and never started.
5. **BcMedia mapping.**

   | BcMedia | SourceUnit |
   |---|---|
   | `Iframe` / `Pframe` | `Video` (`cam_us = microseconds`) |
   | `Aac` | `Aac` |
   | `Adpcm` | dropped, with a one-time warning (R3) |
   | `InfoV1` / `InfoV2` | ignored |

### 3.3 Other neolink changes (complete list, F-int 7)

| Area | Change |
|---|---|
| cfg gates | `any(feature="gstreamer", feature="native-rtsp")` on `usecounter.rs:66,92,148`, `instance.rs:22`, `config.rs:2,104`, `main.rs:43-54,90-139` and `cmdline.rs:20-32`. `src/rtsp/cmdline.rs` `Opt` moves to `src/rtsp_common/cmdline.rs`. `image` is gated at `main.rs:43-54`. That gate is lifted, `mod gst` (`image/mod.rs:30`) and the `use_stream` branch (`:41`) get the gstreamer cfg, and `--use-stream` without gstreamer exits 2 with a message (R15). The `talk` subcommand stays present and exits 2 likewise. |
| Features | `native-rtsp = ["dep:neolink_rtsp"]`, **not in `default` during beta (F-int 2)**. The release CI matrix adds a `native` artifact built with `--no-default-features --features native-rtsp` (no `pushnoti`, R17). |
| Backend select | `rtsp_backend` key plus `--rtsp-backend` CLI. Default `gstreamer` when both are compiled in during beta, `native` after the flip. Choosing a backend that isn't compiled in is a startup error. |
| Config | New keys (§7). The R14 dead-key warnings come from a second pass over `toml::Value`, applied to file **and** MQTT-provided configs (`main.rs:77-81`, `mqtt/mod.rs:205`) (F-int 17). Redacting `Debug` for `CameraConfig`, `UserConfig` and `MqttServerConfig` (F-int 16). The trace at `camthread.rs:40` logs the redacted form. The MQTT `config/status` error echo is redacted. |
| Shutdown (R27, F-int 3) | New `NeoReactor::shutdown(deadline)`: HangUp to each `NeoCam`, await the JoinSet. `camthread` logout gets `timeout(2 s)`. `main` sets up `tokio::signal` (unix SIGTERM/SIGINT; Windows ctrl_c and ctrl_break), then a root `CancellationToken`, then RTSP server shutdown (BYE, `shutdown(Write)`, drain ≤ 1 s, close), then `reactor.shutdown(3 s)`, then exit 0. A hard deadline of 5 s → `exit(0)`. The `mqtt-rtsp` `select!` gets the same token, so one side finishing shuts the other down gracefully (F-int 21). The root tokio features now list `signal`, `net` and `time` explicitly (F-int 20). |
| MQTT | `mqtt_config_writes` (R29) gates the `config` topic handler (`mqtt/mod.rs:194-268`). When it is false, the handler rejects the write and publishes a redacted reason. |
| Docker | **CMD exec form** that keeps `/usr/local/bin/neolink` (F-int 14). `NEO_LINK_MODE` is honoured by argv pre-processing in `main`: if no subcommand is given and the env var is set, it is inserted (clap `env` can't pick a subcommand). `FROM scratch` + CA certs + a `/state` volume. **The image runs as root by default for compatibility (F-ds 15)**: existing setups mount root-owned 0600 configs. `PUID`/`PGID` env vars make the binary drop privileges after reading the config. An unreadable config gives a clear EACCES error that names the file and mode. `state_dir` defaults to `/state` when `NEO_IN_CONTAINER=1`, which the image sets. The `ulimit -n 65535` from `entrypoint.sh` becomes a `setrlimit` call in the binary. `/entrypoint.sh` is kept as a tiny shim in the `:native-debug` (busybox) variant only. Release notes list the removal of `/entrypoint.sh` and the shell. |
| build.rs | Gated on `CARGO_FEATURE_GSTREAMER` and `CARGO_CFG_TARGET_OS`; today it checks the host OS (F-int 19). |
| Release profile | `[profile.release] strip = "symbols"`, keeping `panic = "unwind"`. musl via `cross` with `JEMALLOC_SYS_WITH_LG_PAGE=16` on aarch64. |

## 4. Hub (one task per camera × stream kind)

```
SourceUnit ─▶ Ingest ─▶ MediaClock ─▶ Desc ─▶ Pacer ─▶ WireClock ─▶ fan-out (per-session queues)
                                                         ▲
                                                Bridge ──┘ (synthetic frames, same stage)
```

The Hub is a single task that owns all of its state, so there are no locks on the media
path. Sessions talk to it through a command channel (`Subscribe`, `Unsubscribe`, `Demand`).
Subscribe is processed **between** frame releases inside the same task, which makes it
atomic with respect to sends (F-cc 31). The Hub never awaits a session (F-cc 14).

### 4.1 Ingest

- **Annex-B split.** Handles 3- and 4-byte start codes.
- **NAL whitelist (R2).** Each filtered type is logged once.
- **Access-unit assembly.** One `SourceUnit::Video` is one AU (verified: one BcMedia
  frame per picture).
- **ADTS (F-cc 18).**
  - Strip the 7- or 9-byte header.
  - If there are N > 1 raw blocks and the frame has a CRC, split using
    `raw_data_block_position[]` from `adts_header_error_check`.
  - If there are N > 1 and no CRC, the frame cannot be split, so pass it as one AU with
    AU-size covering all blocks. RFC 3640 allows this only with a `constantDuration`
    change, so instead log it and drop the frame. Measured on the Argus: N = 1 always.

### 4.2 MediaClock: camera time to continuous media time

The MediaClock is per Hub and lives as long as the Hub, **across Idle** (F-cc 16).

**Video**
- `d = cam_us.wrapping_sub(last_cam) as i32`.
- If `0 ≤ d ≤ 2 s` and there is no `Restarted`: `media_us += d`.
- Otherwise it is a **discontinuity**: `media_us += 1 frame` (66 ms default, or the
  measured interval). The event is flagged so that the WireClock re-latches its offset
  (§4.5). The MediaClock itself never looks at host time.

**Audio (F-cc 3, 24; one rule shared with SPEC R19, F-ds 3)**
- Kept in **samples**: `audio_samples: u64`. Each AU advances it by 1024.
- **Anchor**, at the first AU after start or after a discontinuity:
  `audio_media_us = media_us of the latest video frame + skew_estimate`. `skew_estimate`
  starts at 0 (PLAN 0.4 measured a +8 ms median on the Argus) and is persisted in desc.json.
- **Offset estimate**: on each video frame,
  `err = (audio position at arrival) − (video media_us at arrival)`. The estimate is the
  **median over a sliding 20 s window**. The median is used because the arrival-skew sign
  is unknown: bairelay reports video ahead, while GStreamer measured audio 1.9 s ahead.
  Phase 0.4 confirmed it: on the Argus `err` spans −67…+91 ms with both signs, the 20 s
  medians stay within 0–21 ms, and drift is −1.2 ± 25.4 ppm, so on that camera the
  correction below never engages (SPEC §9).
- **Correction**: when `|median| > 80 ms` (this keeps A16's 150 ms with margin), correct at
  up to 1 AU per second until `|median| < 20 ms`. Audio ahead: drop one AU. Audio behind:
  skip 1024 samples, a single RTP delta of 2048. That is the only case where an audio delta
  is not 1024.
- **Initial error**: if the first 2 s of `err` show `|median| > 500 ms`, re-anchor once, as
  a jump, before any session has received audio. Otherwise it is corrected gradually.
- **Monotonic**: audio media time never goes below `last + 1 AU`.
- The `drift_corrections` stat is logged at debug level and exposed in the stats line
  (§4.8).

### 4.3 Desc and learning

**Readiness**
- The desc is ready at the first IRAP, plus either the first audio AU or **1 s** after that
  IRAP, whichever is first.
- With `audio = "none"`, it is ready at the first IRAP itself (F-cc 25).
- Measured on the Argus: audio arrives in the same burst as the IRAP.

**Identity**
- Only `VideoIdentity` and the ASC count (F-cc 4).
- `framerate` is informational. It is the median frame interval over the first 30 frames,
  then frozen.
- **Late audio**: if audio first appears on a desc published without audio, the change is
  recorded for the *next* DESCRIBE and existing sessions are not touched.
- **Codec / resolution / params change**: `gen += 1`. Sessions on the old gen are closed via
  their cancel token (BYE, then close).

**Cache** (`{state_dir}/cache/{sha256(camera)[..16]}-{kind}`; the same file name rule
everywhere, F-int R11)
- `desc.json` holds the SDP inputs.
- `irap.bin` holds the last IRAP AU, if `persist_irap = true` (default true). It is
  written on a desc change, at most hourly, and at shutdown (F-ds 11: SD-card wear). It
  carries the desc hash; an `irap.bin` whose hash doesn't match `desc.json` is ignored.
- Both are written by atomic rename.
- On startup, the cache supplies both the desc and a bridge IRAP before any camera contact
  (F-cc 11).
- `irap.bin` is an image from the camera at rest on disk; this is documented.

**Learning start (R22, F-cc 17)**
- When a mount has no cached desc and the Hub needs one (a DESCRIBE cache miss), it starts
  the source **ignoring `pause.on_motion`**, for at most 1 IRAP + 1 s or 10 s total.
- Then it stops, unless a session is now playing.
- Rate limit: battery (or unknown) cameras get at most one learning start per mount per
  10 minutes. Mains cameras are not rate-limited. There is no separate per-IP limit
  (F-ds 18).

### 4.4 Pacer: when to release (never *what timestamp*)

Media time is ingested in bursts. The pacer releases each frame at
`due = rel_anchor_wall + (media_us − rel_anchor_media) + latency`.

The pacer changes **only release times**. Wire timestamps come from §4.5. Pacer anchor
moves therefore never show up as RTP or NTP steps (F-cc 7).

- **First frame after (re)start** is released immediately. It is an IRAP, because the
  ingest gate drops video until the first IRAP. The anchor is set so this frame is due now,
  which gives clients data at once (F-cc 11). Subsequent frames are spaced by media time.
- **Underrun** (`arrival > due + 1 frame`): the anchor moves forward so the frame is due on
  arrival. Delivery latency grows; timestamps are unaffected.
- **Excess latency recovery (F-cc 8)**:
  - Track `lead = due − now` of the newest frame, and keep the **minimum** over a 10 s
    window.
  - If `min_lead > latency + 1 frame` for 10 s, pull the anchor back by
    `min_lead − latency`. That is one shift, which means a burst of early releases, which
    is harmless.
  - If `min_lead > latency + GOP`, skip queued frames up to the newest IRAP. This drops
    media without changing timestamps; the wire shows a gap, handled as in §4.5.
- **No ppm slew is needed.** Long-term camera-vs-host drift shows up as a slowly growing or
  shrinking `min_lead`, and the rule above corrects it.
- **`latency = 0`**: release on arrival. Per-track monotonicity is enforced by the
  WireClock (F-cc 32).
- **Default `pacing_latency_ms`**: 1500 when battery or unknown, otherwise 300.
  Spec §9 Q4 decides whether mains-powered cameras burst.
- **Congestion episodes (SPEC §9, 2026-10-11).** The Argus link sometimes slows for tens of
  seconds; the camera buffers and arrivals fall up to 17 s behind, with no frames lost.
  The rules above handle it without new mechanism: every late frame is an underrun, so the
  anchor follows arrival and delivery latency grows (timestamps don't move); when the
  catch-up burst arrives, `min_lead` exceeds `latency + GOP` and the pacer skips to the
  newest IRAP, back to `latency`. Arrival gaps stay below the bridge threshold, so viewers
  see late video, then one skip, and no bridge at the battery default latency (1500 ms,
  threshold 4.5 s). The real capture's longest gap was 3.1 s; the synthetic
  `congestion_h265.bcm` fixture (`cargo xtask mkfixture`; 14 s peak lag, 4.3 s longest
  gap) is harsher. At the 300 ms mains default (threshold 3.3 s) such a gap would bridge
  briefly, which is acceptable: Phase 2.6 tests both latencies on that fixture.

### 4.5 WireClock and Bridge: the only source of on-wire time

There is one wire clock per Hub, in µs, monotonic, shared by all tracks and sessions:
`wire_us = media_us + offset`. Every released or synthetic frame gets
`wire_ts = wire_us` at release.

**Offset rules (F-cc 1, 6)**

| Event | New offset |
|---|---|
| Hub created | `0` |
| Bridge engaged, then live resumes | `offset = virtual_now − media_us(first live frame)`, where `virtual_now = last_wire_out + (now − last_wire_out_wall)`. Applied to all tracks. Live video is dropped until the next IRAP (that IRAP *is* the "first live frame"). |
| MediaClock discontinuity flag | Same formula as resume |
| Otherwise | unchanged |

**Monotonic guard.** Per track, `wire_ts = max(wire_ts, last_out + 1 tick)`. This is
enforced after the offset is applied. The guard is a safety net; any time it engages it is
counted and logged as a bug.

**Per-session guard (F-ds 7).** Each session remembers the last `wire_ts` it sent per
track. Video below that is skipped until the next IRAP above it; audio below it is skipped.
This makes late-joiner restamps safe even during a pacer underrun.

**Bridge (R21)**
- Engages when no live video has been *released* for **3 s + latency**, measured from the
  pacer's perspective (F-cc 6). With latency 1.5 s that is 4.5 s, so normal Argus arrival
  gaps (p99 0.9 s, max 3.1 s over 10 min, SPEC §9) never trigger it.
- While bridging, at `bridge_fps` (default 0.2, i.e. one IRAP every 5 s; F-ds 11):
  - `wire_ts = virtual_now` (it tracks wall time, F-cc 6).
  - The frame is the cached IRAP AU, followed by an **end-of-sequence NAL** (H.265 type 36,
    H.264 type 10) so every bridged picture starts a fresh coded video sequence. That avoids
    a duplicate POC or `idr_pic_id` (F-cc 19).
- **Audio while bridging (F-cc 12)**:
  - Send a canned **silent AAC-LC AU** at the AU cadence, with `wire_ts` following
    `virtual_now`.
  - The silent AU's bytes are a function of channel count only. They are generated once by
    ffmpeg in phase 0 and embedded, with a unit test that ffmpeg and symphonia decode it to
    zeros.
  - Live audio is dropped while bridging; audio re-anchors on resume (§4.2).
  - The silent audio AUs keep RTP flowing at about 16 packets/s between bridged IRAPs.
    That satisfies VLC's "no data for 10 s" check and go2rtc's 5 s read deadline.
- **Exit**: on the first live IRAP release, the offset is re-latched (table above).
- **`bridge_max` (F-cc 13, F-ds 1)**: the cap is driven by `SourceUnit::State`.
  - While `Retrying` (outage), bridging is limited to `bridge_max_s` (default 120). After
    that, sessions get BYE and close, and the Hub stops retrying until a new PLAY.
  - While `Paused` (`pause.on_motion` idle), bridging is unlimited, because that is a
    deliberate pause.
- **`use_splash = false`** disables bridging. Sessions then just receive SR keepalives
  (§5.3) during gaps. That is the user's choice and is documented as client-unfriendly.

**Bandwidth.** At 0.2 fps, bridging costs about 1.2 Mb/s per client for 4K main (763 KB
every 5 s), versus about 6 Mb/s at 1 fps. A12 measures whether Frigate tolerates 0.2 fps
and Phase 5 tunes the default. A recording NVR stores the still frame for the whole pause;
this is documented next to `pause.on_motion`.

### 4.6 Fan-out with per-session byte budgets (F-cc 9, 10)

`tokio::broadcast` is replaced. The Hub holds `Vec<SessionTx>`, and each entry is a bounded
`mpsc::Sender<Arc<Frame>>` paired with an `AtomicUsize` of queued bytes.

**Send**
- For each session, if `queued_bytes + frame.bytes > budget`, the frame is not sent.
- The session is then marked `resync` and gets **no video until the next IRAP** and no
  audio until that IRAP's timestamp. The writer's late-joiner path does the rest.
- `try_send` is used, so the Hub never awaits.
- Resyncs are counted per session. 3 within 60 s close the session (R13).

**Budget**: `max(4 MiB, 2 × bytes of the largest GOP seen)`. A GOP is roughly 1 GOP
interval plus the pacing latency, so this also meets SPEC R13's time-based sizing. Measured
Argus 4K main: IRAP ≤ 763 KB plus 29 P-frames, so a GOP is about 1.5–2.5 MB and the budget
is about 4–5 MB per stuck session (F-ds 18).

**Memory**: the worst case is (number of stuck sessions) × budget. There is no shared ring
for one stuck client to pin.

**Late joiner (R12)**: `Subscribe` replies with the desc, a restamped copy of the cached
IRAP, and the session's sender registration.
- The restamp is `wire_ts = last_wire_out` (no extrapolation, F-ds 7), plus EOS.
- Live video then flows from the next IRAP. The per-session guard makes ordering safe.
- Audio is gated to `≥` that restamped IRAP.
- **Cold PLAY** (no cached IRAP and nothing released yet): the first frame this session
  receives is the first live IRAP. `RTP-Info` is still computed from it, because PLAY's
  reply is held until the Hub has a first frame or 2 s have passed (F-ds 18). On timeout,
  RTP-Info omits `rtptime`, which RFC 2326 allows.

### 4.7 Hub states and demand (F-cc 2, 13, 16, 26)

```
 Idle ──PLAY/learning──▶ Starting ──first IRAP──▶ Live ──no video 3 s+latency──▶ Bridging
  ▲                        │  start error                      ▲                    │ live IRAP
  │ demand 0 for 4 s       └──────────▶ Retrying (backoff 1..30 s) ─────────────────┘
  └─────────── (any) ◀── bridge_max exceeded / no sessions
```

- **Demand** is the number of sessions that have **PLAYed** and are not paused.
  DESCRIBE and SETUP are **not** demand (F-cc 2).
- **DESCRIBE on a cache hit**: answered immediately, and the camera is not touched.
- **DESCRIBE on a cache miss**:
  - Mains cameras: start a learning start and wait ≤ 2 s.
  - Battery or unknown cameras: the same, but rate-limited to one learning start per mount
    per 10 minutes (§4.3).
  - Otherwise answer 503 with `Retry-After: 10`.
- **Linger**: 4 s after demand drops to 0. This fits A8's 5 s including the stop RTT.
- **Retry**: backoff from 1 s up to 30 s while demand > 0, capped by `bridge_max`. A failed
  start that isn't followed by a successful one within `bridge_max` puts the Hub in Idle.
- **Hub panic**: the supervisor restarts the Hub with backoff from 1 s up to 60 s. After
  5 panics in 10 minutes it stops trying and the mount answers 503; a log error names the
  panic (F-cc 27). parking_lot locks don't poison.

### 4.8 Observability (R28)

- **Info-level events** carry `camera`, `kind` and, where relevant, `peer`:
  - connection open/close (peer IP, user, path);
  - auth failure (peer IP, username tried; never the password);
  - Hub state transitions (Starting / Live / Bridging / Retrying / Idle);
  - MediaClock discontinuity and re-latch;
  - slow-client resync and drop.
- **Stats line**: one per Hub. Fields: clients, video bitrate, fps, drops, resyncs,
  `drift_corrections`, `guard_engaged`, pacer slips, bridge seconds.
  - Logged at debug every 60 s, and at info on SIGUSR1 (unix).
  - The same counters back the `metrics` feature (§9) and the test-only task/use-counter
    dump used by Gate 1 (PLAN 1.10).

## 5. Media to RTP

### 5.1 Packetizers (ported from bairelay `codec/*`, checked against gortsplib vectors)

- **H.264** (RFC 6184): single NAL if `len ≤ mtu − 12`, else FU-A. The marker goes on the
  last packet of the AU.
- **H.265** (RFC 7798): single NAL or FU (type 49), with the PayloadHdr built from the
  original layer and TID. S and E are set by fragment index. No AP.
- **Parameter sets** (`parameter_sets = "strip" | "passthrough" | "inject"`, F-ds 6):
  - `strip` is the default (confirmed by A4b at Gate 3, SPEC §9). It removes in-band VPS/SPS/PPS, including the
    camera's own copies (the Argus sends them with every IRAP), and relies on the SDP. That
    is bairelay's fix for the HA `ffmpeg:` re-publish → go2rtc `frame.jpeg` exit-183 bug.
  - `passthrough` forwards the camera's in-band copies unchanged.
  - `inject` guarantees VPS/SPS/PPS as separate single-NAL packets at the IRAP's timestamp
    before every IRAP, deduplicated against the camera's copies.
  - A4b at Gate 3 confirms the default. A7 checks that a joiner can decode from the SDP
    alone (`strip`) and with in-band sets (`inject`).
- **AAC** (RFC 3640 AAC-hbr): one AU per packet. AU-headers-length = 16, then AU-size
  (13 bits) and AU-index (3 bits) = 0. The marker is always set. AUs larger than
  `mtu − 16` are fragmented per §3.2.3.

### 5.2 Per-session stamping

- Each session has, per track: a random SSRC, a random initial sequence number, and a random
  RTP timestamp offset.
- `rtp_ts = (wire_ts × clock / 1e6 + offset) mod 2³²`, using 90 kHz for video and the
  sample rate for audio.
- For audio, `wire_ts` is derived from the sample counter, so the RTP delta is exactly 1024
  every time (F-cc 24).

### 5.3 RTCP (F-cc 7, 11)

**SR**
- Mapping for every session and track: `NTP = ntp_epoch + wire_us`, where `ntp_epoch` is
  fixed at Hub creation from the system clock.
- RTP and NTP are both pure functions of `wire_us`, so the SR slope equals the RTP slope by
  construction. Offset re-latches move both together, and both tracks share the same
  `wire_us`, so ffmpeg's SR-based pts stays A/V-consistent.
- The SR ticker starts **at PLAY**, sending every 2 s and immediately at PLAY. When no frame
  has been released yet, it uses `virtual_now` for `wire_us`.

**When `rtcp_sr = false`**: the ticker sends an RR-only compound with an empty report every
2 s as a keepalive. Something always reaches the client within 2 s, which go2rtc's 5 s read
deadline needs.

**RR from the client**: liveness plus loss stats.

**BYE**: sent on close (§6.2).

### 5.4 SDP (matches golden transcripts, spec §9)

Header block, unchanged from GStreamer apart from the tool name:

```
v=0
o=- {random u64} 1 IN IP4 {server ip}
s=Session streamed by neolink
i=rtsp-server
t=0 0
a=tool:neolink
a=type:broadcast
a=control:*
a=range:npt=0-
```

Video media section, H.265 case:

```
m=video 0 RTP/AVP 96
c=IN IP4 0.0.0.0
a=rtpmap:96 H265/90000
a=fmtp:96 sprop-vps=…;sprop-sps=…;sprop-pps=…
a=framerate:15
a=control:stream=0
```

For H.264 the `rtpmap` is `H264/90000` and the fmtp is
`packetization-mode=1;sprop-parameter-sets=…;profile-level-id=…;level-asymmetry-allowed=1`.

Audio media section:

```
m=audio 0 RTP/AVP 96
c=IN IP4 0.0.0.0
a=rtpmap:96 MPEG4-GENERIC/16000/1
a=fmtp:96 streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=…
a=control:stream=1
```

Payload type 96 on both m-lines, as GStreamer does today.

Deltas from GStreamer (listed in spec §5):
- `s=` and `a=tool` say neolink.
- `a=ts-refclk`, `a=mediaclk` and `a=ssrc` are dropped. The SSRC is still given in the
  SETUP Transport header.
- `a=framerate` is added.
- The audio codec changes.

## 6. RTSP layer

### 6.1 Parsing

- **Own parser** for `METHOD SP URI SP RTSP/1.0` plus headers. Header names are
  case-insensitive, values are trimmed, and the head is limited to 8 KiB.
- **Tolerant headers**, ported from bairelay `rtsp/transport.rs`:
  - **Transport**: a list of specs, case-insensitive. **The first acceptable TCP spec is
    chosen and exactly one is echoed**, with `RTP/AVP/TCP;unicast;interleaved=a-b;ssrc=X;mode="PLAY"`
    as GStreamer does. If the list has no TCP spec, the reply is 461 (R4).
  - **Session**: `id[;timeout=n]`.
  - **Authorization**: Basic and Digest, values quoted or unquoted.
- **Interleaved frames from the client**: only RTCP on channels set up for this session is
  accepted. Other frames are skipped after reading `len` bytes.

### 6.2 Connection task

The connection task owns the read half. A writer task owns the write half.

**Writer inputs**
- A **control** channel (bounded 64): responses, RTCP and BYE. The reader awaits it; the Hub
  never touches it.
- The session's **media** receiver.
- A **cancel** token, used for Hub-initiated close and shutdown.

**Writer loop (F-cc 15)**
- It sends media in **chunks of about 64 KiB at packet boundaries**, using `write_vectored`
  over `[$ ch len][12-byte header][prefix][body]` slices. Nothing is copied into a
  per-client buffer (F-cc 10).
- Between chunks it drains the control queue and lets a due audio frame go ahead of the
  remaining video chunks. Interleaved framing allows this.
- **Write stall**: a chunk not written within 10 s closes the connection.
- **Socket options**: `TCP_USER_TIMEOUT 20 s` on Linux, plus keepalive idle 30 s, interval
  10 s, count 3 (F-cc 21).

**Close sequence (F-cc 22)**: queue BYE, then `shutdown(Write)`, then drain reads for up to
1 s, then drop. This avoids an RST discarding the BYE.

**Idle connections**: a connection with no PLAYing session and no request for 60 s is closed
(F-cc 21).

### 6.3 Session

```
 Init ─SETUP─▶ Ready ─PLAY─▶ Playing ─PAUSE─▶ Ready ─TEARDOWN/close─▶ Closed
```

- **Session ID** (R23): 64 random bits, hex. Bound to the connection and to the
  authenticated user. A request carrying another connection's session ID gets 454. Every
  request is authorized again.
- **SETUP**: one per track control URL. Interleaved channel pairs are unique per connection,
  and a requested pair already in use gets the next free pair.
- **PLAY**:
  1. If `desc.gen` ≠ the gen DESCRIBEd, reply **503 with `Retry-After: 0`** and close
     (F-cc 29).
  2. Otherwise send `Subscribe` to the Hub.
  3. Reply 200 with `RTP-Info` computed from the first frame this session will receive (the
     restamped IRAP).
  4. Start the SR ticker, then media.
  - `Range` is accepted and ignored. The reply carries `Range: npt=0-`.
- **PAUSE (F-cc 30)**: the reader sends `Pause` to the writer. The writer unsubscribes,
  discards any partially queued frames, and *then* emits the 200, so nothing is written
  after the response.
- **Response headers**: `Server: neolink` is added. `Public` lists OPTIONS, DESCRIBE, SETUP,
  PLAY, PAUSE, TEARDOWN, GET_PARAMETER and SET_PARAMETER; RECORD and ANNOUNCE get 405 with
  `Allow`. The `Session` header carries `;timeout=60`. Keepalives are accepted but not
  required on TCP.

### 6.4 URL routing

- **Normalisation**: percent-decode, strip the query string, strip a trailing `/`, and
  strip a `/stream=N` or `/trackID=N` suffix.
- **Lookup**: exact match in an `ArcSwap<HashMap<String, MountTarget>>`, rebuilt on config
  change and on availability change.
- **Track resolution**: by suffix, or by an absolute URI equal to `Content-Base` + control.
- **Unknown path**: 404.
- **Configured but unsupported stream**: 404 (F-int 8).

### 6.5 Auth (R6), matching golden behaviour

**Allowed set per mount**

| Config | Allowed |
|---|---|
| `permitted_users` contains `anyone` | every configured user (today's meaning) |
| `permitted_users` contains `anonymous` | no credentials needed, plus the other listed users (as GStreamer's default token honours it, `rtsp/mod.rs:317-328`) |
| `permitted_users` is a list | exactly those users |
| `permitted_users` absent, users exist | every configured user |
| `permitted_users` absent, no users | `anonymous` |

**Responses**
- Anonymous allowed: no challenge.
- No credentials, wrong credentials, or an unknown user: **401**. The challenges are
  Digest first, then Basic, filtered by `rtsp_auth`.
- A valid user who is not in the allowed set: **404**, as today.

**Digest**
- The nonce is `b64(ts ‖ HMAC-SHA256(key, ts ‖ client_ip)[..16])`, so it is bound to the
  client **IP**, not the connection. That supports clients that reconnect after a 401
  (F-cc 20).
- TTL is **5 min**, then `stale=true` and the client re-authenticates transparently. That
  bounds replay of no-qop (RFC 2069) responses, which have no `nc`, to 5 minutes from the
  same IP (F-ds 10). SPEC R6 uses the same rule.
- Both qop=auth and RFC 2069 responses are accepted, and `uri` comes from the header.
- `nc` must be strictly greater than the last value seen for that nonce. A per-process LRU
  of 4096 nonces holds them.

**Credential checks**
- Constant-time compare.
- Users are looked up through a constant-time scan, because the user list is small.
- A user with no password matches only the empty password.

**Failed-auth limiting (F-cc 5)**: only requests that carried credentials and failed count.
The initial no-credentials 401 does not. Failures count per (address, username), where
the address is the IP or IPv6 /64 and the username is the one the credentials claim (known
or not; unparseable credentials count as the empty name), and per address across all
usernames. A username is *limited* from an address after 10 failures for that pair in the
last minute, or after 50 (5 × 10) for the address in all, which stops username spraying.
Credentials for a limited username get exactly the answer wrong ones would (401 with a
fresh challenge, or anonymous access on an anonymous mount), also where a right password
would have earned `stale=true` or a 404, so a guesser can't tell a hit from a miss.

Why per username: under rootless podman (slirp4netns, pasta) or docker-proxy, every LAN
client can appear as one address. A client with a stale password for one user then
throttles only that user, not Frigate's; and nobody gets unthrottled guesses at any user.
The one exception is a connection that already authenticated as that user: its session's
keepalives (which carry credentials and are re-authorized, R23) and its SETUP after
DESCRIBE go through, so a brute force of user X from Frigate's address doesn't break
Frigate's running session as X. It does refuse Frigate's *new* connections as X while it
lasts: from one address a guesser and a reconnecting client are indistinguishable.

Nothing sleeps: an earlier design delayed each failure past the limit by its own 1 s slot
inside the request handler, which held the connection's slots, stalled shutdown, and
still answered a right guess at once.

### 6.6 Limits (R24, revised, F-cc 5)

| Limit | Value | Over-limit behaviour |
|---|---|---|
| Global connections | 128 | 503, then close |
| Per IP (IPv6: /64): **unauthenticated** connections (no credentials accepted and no session yet) | 32 | the IP's own oldest unauthenticated connection is closed |
| All **unauthenticated** connections | `max_connections / 2` = 64 | the oldest unauthenticated connection of the IP holding the most is closed (the new connection's own IP if it holds as many) |
| Unauthenticated lifetime (with or without users) | 30 s from connect, or from losing a session | close |
| Per IP: authenticated connections, and connections with a session (also anonymous) | not limited separately (only by the global limit) | — |
| First request | within 10 s | close |
| Request rate | 20/s, burst 40, per connection | 503 |
| Bodies | Content-Length ≤ 4 KiB | 413 |
| Failed auth | 10/min per (IP, username), 50/min per IP | that username's credentials (every username's, past the per-IP ceiling) from that IP are answered as wrong, except on connections already authenticated as that user (§6.5) |
| accept() errors (EMFILE, ENOMEM) | — | retry after 100 ms |

Why close an old connection instead of refusing the new one: every client starts
unauthenticated, so a full table that refuses newcomers locks everyone out, and a flood
that keeps it full needs no more than it already holds. Closing the heaviest address's
oldest connection means a flood only displaces itself; an attacker would need as many
addresses (or /64s) as there are slots to displace other clients, and even then only
clients still authenticating: authenticated and playing clients are never closed this way.
Legitimate clients are unauthenticated for a round trip or two (Digest 401, then
credentials), or until SETUP on an anonymous mount, so the caps don't touch Frigate,
go2rtc or HA on one IP, behind Docker NAT included.

What stays open: with no users configured (or an anonymous mount), any client can take
up to `max_connections` with real sessions, as many legitimate players could. Anonymous
access means trusting the network; configure users to limit it. IPv6 addresses are keyed
per /64, so devices of one home LAN share an address's caps and failed-auth limit; keying
per /128 would let anyone with a /64 (everyone) bypass both.

## 7. New config keys (schema, F-int §5.2)

| Key | Level | Type / default | Notes |
|---|---|---|---|
| `rtsp_backend` | top | `"gstreamer"\|"native"`, see §3.3 | CLI `--rtsp-backend` |
| `rtsp_auth` | top | list, `["digest","basic"]` | Startup warning if `basic` is enabled without TLS |
| `state_dir` | top | path; `/state` in the container, else `$XDG_STATE_HOME/neolink`, else `./neolink-state` | Not writable → memory-only, one warning |
| `limits.*` | top | the §6.6 values | |
| `mqtt_config_writes` | top | bool, `true` | Startup warning if true and users are configured |
| `rtp_mtu` | top | 576–9000, `1400` | |
| `audio` | camera | `"aac"\|"none"`, `"aac"` | |
| `pacing_latency_ms` | camera | 0–3000, auto (§4.4) | |
| `parameter_sets` | camera | `"strip"\|"passthrough"\|"inject"`, `"strip"` (A4b, Gate 3) | |
| `bridge_fps` | camera | 0.1–2, `0.2` | |
| `battery` | camera | bool, auto-detected (`None` → battery) | Overrides detection |
| `rtcp_sr` | camera | bool, `true` | |
| `bridge_max_s` | camera | 10–3600, `120` | |
| `persist_irap` | camera | bool, `true` | |

Rejected at startup by the native backend:
- `certificate`: "RTSPS is not yet supported by the native backend…" (R7). A hot reload
  that adds `certificate` is rejected with the same message, and the previous config stays.
- `tls_client_auth` ≠ `none`: likewise.

## 8. Failure handling

| Failure | Behaviour |
|---|---|
| Camera unreachable at PLAY | Cached IRAP bridged right away, retry with backoff, `bridge_max`, then BYE |
| Wi-Fi stall < 3 s + latency | The pacer absorbs it; no visible effect beyond a delivery delay |
| Longer stall | Bridge; on resume the offset is re-latched at an IRAP; timestamps stay monotonic |
| Camera reconnect | `Restarted` → discontinuity → re-latch; desc re-checked at the next IRAP |
| Desc change | gen bump; sessions on the old gen are closed |
| Slow client | Byte budget → resync at an IRAP; 3 resyncs in 60 s → close |
| Half-open TCP | Write stall 10 s, plus `TCP_USER_TIMEOUT`, plus keepalive |
| Panic in a connection task | Contained, logged; the connection is dropped |
| Hub panic | Supervisor with backoff and a circuit breaker |
| Cache unwritable or corrupt | Memory-only, one warning; a corrupt file is deleted |
| Hot reload removes a camera or stream | Mount removed, sessions closed, Hub and source cancelled |
| SIGTERM | §3.3 shutdown, < 5 s |

## 9. Testing architecture

**Unit tests**
- Annex-B, NAL filter, SPS parsing (width and height), ADTS and ASC.
- Packetizers, with gortsplib vectors.
- MediaClock: wrap, reset, jitter, audio drift with burst-skew generators.
- Pacer: Argus burst replay, verifying the min-lead recovery and that release never stalls.
- WireClock: a property test that `wire_ts` is monotonic per track under random
  interleavings of bridge, resume, discontinuity and pause.
- Digest: RFC 2617 vectors plus go2rtc's no-qop style.
- Parsers via proptest; SDP via golden files.

**Fixture source**
- Replays `.bcm` files (the `crates/core/examples/bcmedia_dump.rs` format, extended with a
  `kind = 0x7f` record meaning `Restarted`) with original or accelerated timing.
- Synthetic fixtures come from `xtask mkfixture`, a Rust xtask rather than shell (F-int
  A17). It uses ffmpeg (lavfi testsrc2 plus a sine with a periodic flash/beep) to produce
  Annex-B and ADTS, then writes the BcMedia framing with an Argus-like burst schedule.
- **Accelerated replay**: run the pacer with a virtual clock (`tokio::time::pause`), so the
  90-minute wrap test (A3/A17) runs in seconds.

**neolink-side tests**
- A mock `NeoInstance` (UseCounter, motion watch, fake `start_video`) tests cancellation
  and permit release (F-int 1), the learning start, and `pause.on_motion` gating (A12).

**Golden transcripts (A19, F-int 15)**
- Recorded once from the GStreamer backend against the real camera.
- Stored normalised under `crates/rtsp/tests/golden/`. The phase-0 capture exists:
  open/auth, main/sub, TCP/UDP.
- Digest rows have no GStreamer reference. Their expected transcripts come from RFC 2617
  plus captures of ffmpeg, VLC and go2rtc against the native server, reviewed by hand once
  and then frozen.

**N2 measurement (F-int 12)**: a `metrics` cargo feature on `neolink_rtsp` records the
ingest→write latency histogram. It doesn't depend on log level, which is
`release_max_level_debug`.

**CI (F-int 13)**
- A new `test.yml` that runs `cargo test --workspace`.
- An MSRV job running `cargo +1.82 check --all-targets -p neolink_rtsp`.
- Clippy rows for `native-rtsp` and `native-rtsp,gstreamer`.
- Fuzz: a 60 s smoke run per target per PR, and a 30 min nightly run.
- A container job (A13/N9): build the scratch image, run it with the fixture camera, then
  `podman stop` must take < 5 s.
- ffmpeg, VLC and go2rtc integration jobs on an image that has them.

## 10. Power management (spec §4.1.1)

The changes are in `src/common/neocam.rs` and `camthread.rs` and are shared by both
backends.

**Connection state machine per camera**

```
 Asleep ──demand (permit acquired)──▶ Connecting ──login ok──▶ Awake
   ▲                                     │ fail: backoff only while demand (P4)
   └──── idle_grace_s after last permit ─┴─────────────── Awake (no permits)
```

- `NeoCamThreadState` starts as `Disconnected` for battery or unknown cameras (P1). Today
  it is hard-coded to `Connected` at `neocam.rs:63`.
- The idle loop is restructured so the grace timer starts at boot and after every permit
  drop, not only after the first use. This fixes `neocam.rs:357-366`.
- `idle_disconnect` keeps its meaning but changes its default:
  - unset and battery/unknown → sleep;
  - unset and mains → always-on, as today;
  - explicitly `false` → always-on, with a warning on battery cameras.

**Passive tasks (P3)**
- `run_passive_task` already avoids connecting.
- The MQTT snapshot, battery and stream-info timers are changed to **skip** while
  `Asleep`, instead of waiting for the camera.
- Their last values are cached with a timestamp.
- **Piggyback hook:** on each `Awake` transition, one battery read and one stream-info read
  run once, and one snapshot runs at the end of the wake (P5, P7), all under the existing
  permit. The Argus answers the first `battery_info` after a wake with **400** (SPEC §9),
  so the battery read retries every 2 s while the permit is held, up to 3 times, and
  never extends the wake on its own.

**Wake ledger (P6)**
- A `Vec` ring of the last 1000 wakes per camera, in memory.
- Each permit carries a reason string, which `run_task` callers pass. Today permits have
  no reason, so this is a signature change on `NeoInstance::permit`/`run_task`.

**Snapshot endpoint (P5)**
- A minimal HTTP/1.1 GET server on `snapshot_port` (same limits as §6.6; failed auth as
  §6.5, per (address, username) with its own counters, answered at once).
- Auth is the shared module from §6.5.
- It serves the cached JPEG and never takes a permit unless `?fresh=1` is allowed by
  config.

**Additions from the power review (spec P9–P19)**

- **`find_camera` for battery or unknown cameras.** Run local discovery to completion (or
  its timeout) first. Only then, if `discovery` permits it, run remote/map/relay.
  `register_address` (`C2R_C`) is never run concurrently with local discovery.
- **Permits require a `Reason`.** The `UseCounter` gets a non-activating `observe()`
  subscription for the idle loop. Boot tasks become `on_awake` hooks, run once per wake
  inside the wake's own permit.
- **`run_task_with_deadline(reason, dur, f)`** replaces `run_task`. The old signature calls
  it with a 60 s default during the migration.
- **Disconnect.** `NeoCamThread` gets an explicit `disconnect().await`: logout with a 2 s
  timeout, then `C2D_DISC` (UDP), then drop. The idle loop and shutdown both call it,
  instead of dropping the `run_camera` future.
- **State file.** A `CameraState` struct, serialized to
  `{state_dir}/camera/<sha256(name)[..16]>.json` with atomic rename, plus an advisory lock
  file (`fs2`-style `flock` on unix; best-effort elsewhere).
