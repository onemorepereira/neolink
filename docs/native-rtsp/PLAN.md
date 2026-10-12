# Native RTSP: Plan

Status: **v2** (2026-10-10). v1 was red-teamed against the whole doc set; v2 re-baselines the
estimates, fixes the sequencing, and counts hardware sessions. Implements
[DESIGN.md](DESIGN.md) v2.1 for [SPEC.md](SPEC.md) v2.2.

## Principles

- **Every phase ends in a gate** with named tests. A phase is done when its gate passes,
  not when its code is written.
- **Each phase lands as its own reviewed commit series** on `feature/native-rtsp`.
  Phase 1 is independently useful and ships with the GStreamer backend.
- **No phase changes GStreamer-backend behaviour**, except Phase 1's fixes. Those are
  verified against the golden transcripts.
- **Red-team at each gate.** Before a gate is declared passed, an independent reviewer
  attacks the diff, with the same brief style as the spec and design reviews: a fresh
  Claude agent with no authoring context, plus the owner for sign-off. Findings are fixed,
  or explicitly deferred in this file.
- **Real-time media tests are robust to shared CI runners.**
  - Timing assertions in CI use virtual time (`tokio::time::pause`) wherever the system
    under test is in-process.
  - Wall-clock tests with external clients (ffmpeg, VLC, go2rtc) assert on correctness:
    decode errors, reconnects, monotonicity. Their timing thresholds are 3× the spec and
    get one retry. Exact timing thresholds are checked in a nightly job on a dedicated
    runner.

### Hardware budget

The Argus is battery and solar powered.

- Each hardware session streams for ≤ 10 min.
- Battery % is logged before and after (`neolink battery`).
- No session when the battery is below 40 %.
- **Calendar assumption: at most 2 sessions per day** (October solar). A failed hardware
  gate costs a day, not minutes.
- Long runs (A15 24 h, 2 h runs) need **mains power**. Without it, they run on fixtures and
  the hardware part is skipped, with a recorded deferral.

Estimates are focused engineering days (d) for one developer, excluding review waits and
hardware calendar time. Ranges are realistic: bairelay's equivalent code is about 11.7k
lines (RTSP crate) plus 5.4k (pacer/bridge), and this design is more ambitious in the Hub,
clocks and SR.

## Phase 0: Facts, tooling, scaffolding (4–6 d; 2 hardware sessions)

| # | Task | Output |
|---|---|---|
| 0.1 | ✅ Capture tool `crates/core/examples/bcmedia_dump.rs` | Done; writes `neolink_core::fixture` records, reconnect mode, battery readings |
| 0.2 | ✅ Argus characterisation (spec §9) | Done |
| 0.3 | ✅ Raw golden transcripts: open + auth, main + sub, TCP + default (UDP) | Superseded by 0.3b |
| 0.3b | ✅ **Golden tooling**: `cargo xtask golden record` (recording TCP proxy), `normalize`, and a **structural** `diff` (methods, status codes, header presence and shape, SDP attribute sets and fmtp keys; not sprop, ssrc, seq, rtptime, dates, resolution). Session 2 (2026-10-11) re-recorded all 19 aliases, extern, unknown path, UDP and five auth cases with the pinned ffmpeg | 28 files in `crates/rtsp/tests/golden/gst/` (normalised, no media), `docs/native-rtsp/golden-session.sh` |
| 0.4 | ✅ 570 s main capture (session 1, 2026-10-11). Audio-vs-video `err` median +8 ms (p5 −35, p95 +48), drift −1.2 ± 25.4 ppm; camera vs host −9.6 ± 25.8 ppm (both with start-up/congestion windows excluded and autocorrelation-widened intervals; the first published figures came from an uncommitted fit and were corrected after the Gate 0 review); **new: congestion episodes with up to 17 s arrival lag** (spec §9). Median estimator confirmed; `skew_estimate` = 0 | `docs/native-rtsp/analysis/drift.py`; spec §9 |
| 0.5 | ✅ Logout, 180 s idle, reconnect (same session): `cam_us` continues; wake 2.0 s; first battery read after wake → 400 | `argus_main_10min.bcm` (175 MB, with a `Restarted` record) kept locally, too large to commit; CI uses the synthetic variants |
| 0.6 | ✅ Silent AAC-LC AU, mono `01 18 20 07` and stereo `21 10 04 60 8c 1c`, sample-rate independent; symphonia and ffmpeg decode them to zeros | `neolink_rtsp::codec::aac` + tests |
| 0.7 | ✅ `cargo xtask mkfixture`: plain, wrap, reset, outage, reschange, **congestion** (added from 0.4). Deterministic and CPU-independent (ffmpeg `-cpuflags 0`, x264 `cpu-independent`, x265 `asm=none`). Deviation: wrap starts 10 s before the u32 wrap, not 5 min (configurable with `--cam-us-start`) to keep fixtures small | `fixtures/synth/SHA256SUMS`; `.bcm` files are generated, not committed |
| 0.8 | ✅ `test.yml`: `cargo test --workspace` with the pinned BtbN ffmpeg (`autobuild-2026-10-10-13-04`, n8.1.3, sha256-checked, cached; not fully static, it links the host glibc, and its output was identical under glibc 2.36 and 2.43), the fixture reproducibility check, and an MSRV 1.82 job covering every crate except pushnoti (the binary without GStreamer). tokio 1.37 → 1.42.1 (the newest that keeps MSRV 1.82); `rust-version = "1.82"` on every crate | **Owner decision (2026-10-11): no remote CI until much later.** The same commands run in the dev container and on rust:1.82 and pass; that is what Gate 0 accepts. BtbN deletes old autobuilds, so the tarball must be mirrored before the first remote run (risk register) |
| 0.9 | ✅ Owner decisions (below) | Recorded |
| 0.10 | ✅ NFRs confirmed against 0.4 and the golden data. N3 restated for the CI runner (no Pi 4), N5 runs on fixtures (no mains). N1/N2/N4 unchanged | Spec v2.3 |

Hardware used: 2 sessions on 2026-10-11 (battery 55 % → 53 %). Session 2 kept the camera
connected for 11 min instead of about 5: the script stopped neolink with SIGINT, which a
non-interactive `sh` ignores in background jobs, so neolink kept running until it was sent
SIGTERM by hand. The script now uses SIGTERM.

**Gate 0** (passed 2026-10-11, after the review findings below were fixed)
- ✅ Spec §9 Q1, Q2 and Q5 are answered.
- ✅ Fixtures are generated reproducibly (`mkfixture --standard --check`), in the dev
  container instead of remote CI (owner decision 2026-10-11).
- ✅ `cargo test --workspace` passes, likewise in the dev container.
- ✅ The golden tooling diffs a re-recorded GStreamer transcript against itself: 0 structural
  deltas (`crates/xtask/tests/golden_files.rs`).

Gate 0 review (fresh agent, 2026-10-11), fixed: the camera-vs-host drift figure was not
reproducible from the committed script, and the audio interval was too narrow (both
recomputed); synthetic audio lagged video by the encoder's 64 ms priming (first AU now
dropped, audio trimmed to the video length); the outage variant silently broke when the
outage ran past the media (now an error); the structural diff missed fmtp values, CSeq
echo, session-id consistency, PT consistency, channel counts, status line version and
reason, media flow (counts now bucketed `x1`/`x10+`), and hid TEARDOWN errors (now only a
reply missing on one side is ignored); the splitter now resyncs; re-records are byte-stable
(interleaved lines sorted); ADTS rejects multi-block frames and IRAP covers types 22–23;
fixture records are capped at 16 MiB; the congestion window is in host time; the proxy
writes a ready file and the recording script waits for it and edits the config safely;
the MSRV job covers more crates and CI no longer runs twice per push. Deferred: folded
RTSP headers are not parsed (no client sends them); the proxy's Down pump does not end if
a server never closes after the client half-closes (the GStreamer and native servers do).

## Phase 1: neolink refactors, test seam, fixes (8–12 d; 1 hardware session)

Useful on its own; it **ships with the GStreamer backend**.

| # | Task | Design ref |
|---|---|---|
| 1.1 | PR the PIR fix (`48de292`, already committed on `fix/pir-firmware-v3`) separately; rebase `feature/native-rtsp` onto it once it is merged | — |
| 1.2a | **neolink_core `test-util` feature**: `BcCamera::from_parts`, a scripted Baichuan peer (login, keepalive, abilities, video start/stop, `.bcm` replay, disconnect injection, motion events) | §3.2 test seam |
| 1.2b | First tests in the neolink binary, using the peer: a `NeoInstance` over the fake camera | §3.2 |
| 1.3 | Cancellable `stream()`/`stream_while_live()`, a single spawn with a kept `JoinHandle`, `StreamEvent { Media, Restarted, State }`; the GStreamer factory ignores the new variants; test `stream_cancel_releases_permit`; regression test for the on_motion task leak | §3.2 (1–2) |
| 1.4 | `init_permit` parameter (GStreamer passes 5 s); `pause.on_motion` re-read per start | §3.2 (3) |
| 1.5 | `NeoReactor::shutdown`, logout timeout, `tokio::signal`, root cancel token, `mqtt-rtsp` graceful select | §3.3 |
| 1.6 | Redacting `Debug` (CameraConfig, UserConfig, MqttServerConfig); redacted `config/status` echo | §3.3 |
| 1.7 | Dead-key warnings via the `toml::Value` pass (file and MQTT); a **config-corpus parse test** (R14) built from `sample_config.toml`, the README examples, and a few synthetic legacy configs | §3.3 |
| 1.8 | `build.rs` gating; explicit tokio features; `strip = "symbols"`; release CI uses explicit feature lists instead of `--all-features` | §3.3 |
| 1.9 | Docker: exec-form CMD, `NEO_LINK_MODE` argv pre-processing, `setrlimit`, entrypoint without the `exit 1` trap; still root and still the Debian image in this phase | §3.3 |
| 1.11 | **Power (spec §4.1.1), part 1.** P1 (no connection at boot; sleep by default for battery/unknown cameras; explicit always-on with a warning), P2 (`idle_grace_s`, default 10 s, applied to the startup connection too), P3 (periodic tasks never connect; cached values with age), P4 (backoff only with demand), P6 (wake ledger and summary line), P7 (battery piggyback). These fix today's GStreamer neolink as well. | §10 |
| 1.13 | **Power, part 3** (power review): P9 (local-first discovery, never parallel with cloud), P10 (no reasonless or boot-time permits; subscription-based idle observer), P11 (`run_task` deadlines, demand definition, MQTT coalescing), P12 (motion permits gated, reset and capped), P13 (logout + `C2D_DISC` on every path; drop the fixed sleeps; remove the bogus probe login), P14 (MQTT config/retain/availability), P15 (state file, lock file, CLI reads the state), P16 (GStreamer mounts from state), P17, P18 | §10 |
| 1.14 | Fix the `Thread panicked` on CLI exit (observed 2026-10-10) as part of 1.5 shutdown | §3.3 |
| 1.12 | **Power, part 2.** P5 cached snapshot endpoint (a small HTTP/1.1 listener sharing the RTSP auth module), P8 warning | §10 |
| 1.10 | Test-only stats dump (task count, per-camera use-counter) on SIGUSR1 behind `cfg(any(test, feature = "debug-stats"))` | §4.8 |

**Status (2026-10-11).** Done on `feature/native-rtsp`; how each task landed:

| # | Result |
|---|---|
| 1.1 | Not applicable: no upstream PRs (owner decision 4). The PIR fix is in local `master` with Phase 0 |
| 1.2a | `neolink_core::test_util::FakeCamera` (TCP, unencrypted): login, abilities, ping, version, UID, stream info, battery (with busy 400s), time, snapshot, motion events, `.bcm` replay; counts connections, logins, logouts, messages; drop/unreachable injection. `BcCamera::from_parts` was not needed: the fake is reached through the real `BcCamera::new` |
| 1.2b | 29 tests in `src/common/tests.rs` drive `NeoCam` against the fake; `cargo xtask fakecam` runs it as a process for `tests/e2e/*.sh` |
| 1.3 | `StreamRx`: dropping it aborts the stream task and every `pause.on_motion` helper (one task leaked per session before); `StreamEvent { Media, Restarted }` |
| 1.4 | Already true: the 5 s initial permit is in `stream_while_live` and `pause.on_motion` is read on each start. No change |
| 1.5, 1.14 | SIGTERM/SIGINT → `NeoReactor::shutdown` (logout, close) → explicit exit. `podman stop` 155–172 ms, exit 0, one logout. No "Thread panicked" |
| 1.6 | Config `Debug` is the serialized form (secrets skipped); test |
| 1.7 | Unknown-key warnings (file and MQTT); key lists checked against the structs; config corpus test (sample config, README blocks, legacy aliases) |
| 1.8 | `build.rs` keyed on `CARGO_FEATURE_GSTREAMER`/target OS; `strip = "symbols"` (17.8 → 13.0 MB); explicit release features |
| 1.9 | Exec form, neolink is PID 1, `NEO_LINK_MODE` and `RLIMIT_NOFILE` handled in-process, state volume. Still root and Debian |
| 1.10 | Feature `debug-stats`: SIGUSR1 logs tasks and per-camera permits/wakes |
| 1.11, 1.13 | P1–P4, P6, P9–P16 implemented (power increments A–C). P13's C2D_DISC is sent on every deliberate UDP close. Deferred: P11's MQTT query coalescing (each query is bounded by the 60 s deadline instead); P17/P18 belong to the native Hub (Phase 4) |
| 1.12 | P5 snapshot endpoint (Basic auth; Digest comes with the native auth module), P7, P8 |

Found and fixed along the way: requests on a dropped TCP connection hung until five ping
timeouts; `logout()` was lost because shutdown cancelled the writer first; the TCP probe
logged in as admin/123456 on every connection; GStreamer published no mounts for a
sleeping camera, so no client could wake it; the motion watcher of every `on_motion`
session leaked; motion `Start` survived a disconnect and could hold a permit.

**Gate 1** (status 2026-10-11)
- ✅ `podman stop` < 5 s with exit 0 on the GStreamer image (N9): 155–172 ms (`tests/e2e/container.sh`).
- ✅ **100 connect/teardown cycles against the scripted peer**: the use-counter returns
  to 0 after *each* cycle, and the task count returns to baseline (`test_100_cycles_return_to_baseline`).
- ⏳ On hardware, 10 cycles with a 6 s gap (1 session): the use-counter returns to 0 each
  cycle, checked with the 1.10 dump. **Deferred to the next day**: 2026-10-11 already had
  four camera sessions against the budget of two.
- ✅ **A23–A26 and A28–A30 pass against the scripted peer** (`src/common/tests.rs`, `tests/e2e/power.sh` with the real `neolink mqtt-rtsp`, mosquitto and ffmpeg). The fake is TCP-only, so "0 discovery broadcasts / 0 cloud registrations" is shown by 0 connections with UDP discovery unused; a UDP/p2p mock is deferred to Phase 4.
- ✅/⏳ **A27 on hardware**: 2026-10-11 15:54 UTC, 60 s of sub: 0 connections at boot, one wake of 79.3 s, battery read on the deferred retry (50 %), then no contact during the idle checks (running at the time of writing).
- ✅ The golden structural diff against the GStreamer backend is clean: on the fake camera the Phase 1 binary is identical to the pre-Phase-1 binary and both match the goldens (`tests/e2e/golden-fake.sh`; GStreamer's pre-existing PLAY 503 for back-to-back clients is retried and counted).
- ✅ No password appears in the most verbose release log (trace is compiled out; debug checked in `power.sh`) or in config `Debug`.
- ✅ The config corpus parses.
- ✅ Red-team review of the Phase 1 diff (fresh agent, 2026-10-11). 20 findings:
  - **Fixed, with tests:**
    - switching always-on → sleep never disconnected;
    - the 10-min slow-retry clock wasn't reset after a good session;
    - concurrent state-file writes lost fields (shared temp name, no lock);
    - shutdown during a backoff or connect took the full 5 s;
    - the snapshot endpoint leaked camera names (404 before auth).
  - **Fixed:**
    - `NEO_LINK_MODE` broke `--version`/`--help` and old compose `command:` lines;
    - push registration woke the camera at boot (now passive), and each push took a 30 s permit even without a motion consumer;
    - the GStreamer `on_motion` path ignored Unknown and had no hold cap;
    - the 60 s deadline killed `talk` and `image --use-stream` (now stream tasks);
    - `?fresh=1` couldn't finish within the 10 s request timeout (headers keep 10 s, the request gets 75 s);
    - `check_tcp` accepted any listener (it now sends the credential-free login-upgrade request and needs a Baichuan reply);
    - a read-only state dir aborted the wake reads;
    - a CLI run took over and deleted the daemon's lock;
    - the snapshot endpoint now uses constant-time password checks, a real `Content-Length` on HEAD, percent-decoded names and a 503 over the connection cap;
    - clean MQTT hang-up published `disconnected` where HA now expects `offline`;
    - reason de-duplication;
    - the motion permit lingered 30 s after a disconnect and reconnected.
  - **Mitigated:**
    - local-first discovery delayed cloud users by up to 15 s (local is capped at 8 s when the cloud is allowed);
    - mains cameras without `battery = false` now sleep, which is the intended P1 default: a startup info line says how to keep them connected.
  - **Release notes (behaviour change):**
    - HA availability moved to `neolink/status`; `neolink/<cam>/status` drives the new Awake sensor;
    - retained `neolink/config` is ignored at startup.
  - **Deferred:**
    - the ledger counts from the connect request, not the login;
    - an abandoned mid-handshake login skips `C2D_DISC`;
    - per-IP limits on the snapshot endpoint;
    - a UDP fake for power tests (Phase 4; the UDP path is covered by A27 on hardware);
    - Docker `--user` state-dir ownership (Phase 6 image work).

## Phase 2: `crates/rtsp` media core, no network (12–16 d; 0 hardware)

| # | Task | Design ref |
|---|---|---|
| 2.1 | Crate skeleton, deps pinned for MSRV, MSRV CI job (lib only) | §1 |
| 2.2 | `codec/`: Annex-B, NAL whitelist, SPS width/height (H.264/H.265), ADTS→ASC, ADTS block split; **fuzz targets for Annex-B and ADTS** (N7) | §4.1 |
| 2.3 | Packetizers: H.264 FU-A, H.265 FU, AAC-hbr; `RtpChunk` zero-copy; gortsplib vectors | §5.1, §2 |
| 2.4 | MediaClock: video unwrap/discontinuity; audio in samples, median estimator from 0.4 | §4.2 |
| 2.5 | Desc + identity + gen + cache (desc.json, irap.bin with desc hash, write policy) | §4.3 |
| 2.6 | Pacer with virtual-clock tests driven by the Argus burst schedule | §4.4 |
| 2.7 | WireClock + per-session guard + Bridge (EOS NAL, silent AAC, `bridge_fps`, `bridge_max` by `SourceState`) | §4.5 |
| 2.8 | Hub task, byte-budget fan-out, late joiner, demand/linger/learning (`StartMode`), supervisor, stats (§4.8) | §4.6–4.8 |
| 2.9 | Fixture `StreamSource` (`.bcm` replay, real-time and virtual-time); `metrics` feature (N2 histogram) | §9 |

**Gate 2**
- Unit and property tests pass.
- **A22 (wire part)**: `wire_ts` is monotonic per track **and per session** under random
  bridge/resume/discontinuity/pause/underrun/late-join schedules. The SR part is in Gate 3.
- **A17 (clock part)**: wrap and reset fixtures in virtual time give a monotonic wire clock
  with no jump > 1 s. The retina part is in Gate 3.
- With the Argus-like fixture in virtual time: pacer release jitter p99 < 10 ms, and no
  underrun slips in steady state.
- Hub memory with 1 stuck plus 3 healthy subscribers ≤ budget + 10 %.
- Fuzz smoke run: 60 s per target.
- `cargo +1.82 check -p neolink_rtsp` passes.
- Red-team review.

**Status (2026-10-11).** Done on `feature/native-rtsp-p2`. Gate 2 evidence:
- Unit and property tests: 54 pass, repeated runs stable. Clippy `-D warnings` and fmt are clean.
- A22 (wire part): `test_a22_wire_monotonic_property` covers 300 seeds and also requires
  `guard_engaged == 0`: the Hub never relies on the guard.
- A17 (clock part): `test_a17_wrap_and_reset` measures from the first live frame.
- Pacer: `test_argus_like_fixture_is_smooth` (p99 jitter < 10 ms, no steady-state slips) and
  `test_congestion_at_both_latencies`.
- Memory: `test_stuck_session_is_bounded`, with 1 stuck and 3 healthy subscribers, also bounds
  `pacer_queued`.
- Fuzz smoke at 60 s per target: annexb, adts and sps are clean. The first sps run found an i32
  overflow in the scaling list, now fixed with i64 and `rem_euclid`; the crash input replays
  clean.
- MSRV: `cargo +1.82 check --locked --all-targets -p neolink_rtsp` passes.
- Red-team review: 20 findings, all fixed except those deferred below.
  - **Fixed, Hub:**
    - the session guard waits for an IRAP;
    - the bridge resumes only on an IRAP;
    - ingest is gated on the first IRAP;
    - late joiners get the last released IRAP restamped at `last_out_wire`;
    - the source start is non-blocking, with backoff 1–30 s;
    - a single wake covers DESCRIBE, DESCRIBE, PLAY;
    - bridge_max applies before any video;
    - a duplicate subscribe replaces the old session;
    - `Drop` cancels everything;
    - cache writes go through `spawn_blocking`.
  - **Fixed, clock and desc:**
    - corrections begin only after a 2 s span and stay within half an AU;
    - the audio sample rate comes from ADTS (48 kHz test);
    - audio re-anchors stay monotonic after the first audio is heard (the one initial jump may
      go backwards);
    - the cached desc detects an audio change;
    - `irap.bin` is a binary, hash-bound format.
- Re-review of the fixes: two Hub spins, where a learning or linger deadline was left behind
  after the source went away (a failed start, or the source ending during linger), are fixed.
  `test_*_does_not_spin` cover them, use a `wakeups` stat and a watchdog, and fail without the
  fix. A start still in flight is now cancelled if the Hub is gone.
- **Deferred:**
  - availability watching (needs the Phase 4 source);
  - multi-block ADTS without CRC in the desc;
  - B-frames (Argus has none; to revisit in Phase 5);
  - persisting `skew_estimate` across restarts;
  - supervisor panic coverage is test-only;
  - a Learn→Demand conversion does not re-apply `pause.on_motion` (Phase 4, where the policy
    lives).

## Phase 3: RTSP server (10–14 d; 0 hardware)

| # | Task | Design ref |
|---|---|---|
| 3.1 | Request parser plus tolerant Transport/Session/Authorization parsers; fuzz targets | §6.1 |
| 3.2 | Connection task, chunked vectored writer, control priority, close sequence, socket options, idle timeout | §6.2 |
| 3.3 | Session state machine, methods, 461, RTP-Info (including cold PLAY), PAUSE ordering | §6.3 |
| 3.4 | SDP generation; golden structural diff wired into CI | §5.4 |
| 3.5 | Routing (19 aliases, normalisation, ArcSwap) | §6.4 |
| 3.6 | Auth: Basic + Digest (IP-bound 5 min nonce, nc LRU, stale), the 404 rule, `anonymous` in `permitted_users`, failed-auth limiting. Digest oracle: RFC 2617 test vectors, plus **client acceptance** (ffmpeg, VLC and go2rtc authenticate successfully). Not a self-recorded transcript. | §6.5 |
| 3.7 | Limits | §6.6 |
| 3.8 | RTCP SR/RR/BYE and keepalive mode | §5.3 |
| 3.9 | **Client CI image**: pinned static ffmpeg/ffprobe, VLC 3 (`cvlc`), go2rtc release binary, and a separate integration-test crate `crates/rtsp-it` that may use the newest `retina` (it is excluded from the MSRV job, which avoids retina 0.4.13's H.265 profile/tier/level bug) | §9 |
| 3.10 | **A/V-sync detector** for A16: decode with ffmpeg, find flash frames (luma threshold) and beep onsets (RMS threshold), report the offset distribution | §9 |
| 3.11 | Integration tests: retina (TCP; Basic and Digest), ffprobe/ffmpeg, plain ffmpeg UDP→461→TCP, VLC, go2rtc direct, and the go2rtc re-publish chain | §9 |

**Gate 3**, on fixtures in CI
- Pass: A1, A2, A2b, A4, **A4b** (decides the `parameter_sets` default), A5, A6, A7, A8
  (crate part), A9, A10, **A11** (fixture), A16 (5 s, 60 s, 15 min), A17 (retina part),
  A18, A19 (structural), A20, A21, A22 (SR part).
- N1 is measured on fixtures with a simulated wake delay.
- Fuzz smoke run passes.
- Red-team review, including a security pass on auth and limits.

**Status (2026-10-11).** Done on `feature/native-rtsp-p3`. Gate 3 evidence, on fixtures in the
dev container (the same commands as CI):
- **Client image** (3.9): `tests/e2e/clients/Containerfile`. It contains:
  - go2rtc v1.9.14, sha256-pinned;
  - VLC 3.0.24 from deb-multimedia (Debian's VLC has no live555);
  - the pinned ffmpeg.
- **retina** runs as `crates/rtsp-it` with its own lockfile, outside the workspace (retina 0.4.21
  needs Rust 1.88).
- **Clients** (`tests/e2e/clients.sh`):
  - A1: main and sub exact, 15.000 fps, AAC present, the first frame decodes under strip.
  - A2: 120 s each, 1802 video and 1875 audio frames, 0 decode errors, no `RTP: missed`.
  - A2b: UDP gets 461, then TCP.
  - A4: JPEGs.
  - A5: 300 s, 4469 displayed, 0 lost, 0 too late.
  - A10: 21 of 21 (ffmpeg, go2rtc, VLC × 7 rows).
- **A4b** decides `parameter_sets = strip`. Passthrough and inject fail in go2rtc (an ffmpeg AP of
  the parameter sets), and fail the same way without neolink in the chain.
- **A6:** byte-identical over Basic and Digest (retina).
- **A7:** staggered joins, one start per kind, strip and inject.
- **A8:** 100 cycles; the camera stops within 5 s; tasks return to baseline.
- **A9:** a stuck client is dropped and its slot freed; the other client's p99 deviation is
  < 50 ms.
- **A11:** a 60 s outage.
- **A16:** |A−V| worst 35 / 58 / 74 ms at 5 s / 60 s / 15 min, with no drift; start offset
  2–48 ms (GStreamer: 1.9 s).
- **A17** (retina) and **A18**.
- **A19:** 28 of 28 golden cases, only the SPEC §5 deltas (`native-deltas.txt`).
- **A20:** security; plus credentials-in-logs at trace level.
- **A21:** 0 starts over 60 probes.
- **A22:** the SR part, including both tracks on one wall clock.
- **N1** (simulated 3 s wake): first video 3 ms (the persisted IRAP), first live frame 3.005 s;
  while streaming, 2 ms and 1.0 s (the next IRAP).
- **Fuzz:** rtsp_msg, rtsp_headers, annexb, adts and sps, 60 s each, clean. MSRV check passes.
- **Red-team:**
  - The correctness pass found 7 issues plus 2 weak tests, all fixed.
  - The security pass found 10, all fixed. Among them:
    - unauthenticated lifetime and a global cap;
    - failed-auth pacing per IP;
    - Digest uri binding and salted nonces;
    - control characters rejected;
    - no mount enumeration;
    - strict channel pairs.
  - A re-review of the Phase 2 fixes found two Hub spins (fixed before the Phase 2 merge).
  - The long A16 run found two defects, both fixed:
    - looped-fixture timing;
    - the 2-byte H.265 EOS, now sent only after CRA/BLA.
- **Deferred:**
  - the writer does not interleave control or audio between the 64 KiB chunks of one frame;
  - Digest without qop can be replayed by the same IP for the same mount within the nonce's
    5 minutes (the protocol's limit; live555 needs it);
  - the go2rtc AP limitation goes to the release notes.

## Phase 4: neolink integration (6–9 d; 3–4 hardware sessions)

| # | Task | Design ref |
|---|---|---|
| 4.1 | `native-rtsp` feature (not default), `rtsp_backend` + CLI and the default rule, cfg-gate changes, subcommand stubs | §3.3 |
| 4.2 | `src/rtsp_native/`: mounts, `source.rs` (`StreamEvent` → `SourceUnit`, `StartMode`), availability (supported streams, `battery: Option`, persisted, config override) | §3.2 |
| 4.3 | AuthDb from config; hot reload of users, mounts, and camera add/remove | §6.5, §8 |
| 4.4 | New config keys and validation; `certificate` rejection, including on hot reload | §7 |
| 4.5 | `mqtt-rtsp` with the native backend; `mqtt_config_writes` | §3.3 |
| 4.6 | Observability events and the stats line (R28) | §4.8 |
| 4.7 | Scripted-peer tests: A8 (neolink part), A12 (injected MdState, both the 60 s and 10 min variants), learning-start rate limit, `battery: None` treated as battery, A21 against a "battery" fake | §9 |

**Gate 4**
- CI: everything from 4.7, plus every Gate 3 test re-run through the real `source.rs` and
  the scripted peer.
- **Hardware**, 3–4 sessions of ≤ 10 min, in this order:
  1. A1, A2, A7 (main and sub).
  2. A4, A5.
  3. A11, with an nftables drop of 60 s on the neolink host.
  4. A12 (M), with the owner triggering the PIR, plus N1 against a sleeping camera.
- Battery delta is logged for each session.
- Red-team review.

**Status (2026-10-11).** Code and CI part done on `feature/native-rtsp-p4`; the hardware
sessions are pending (camera budget: 4 sessions were used on 2026-10-11).

| # | Result |
|---|---|
| 4.1 | Feature `native-rtsp` (not default); `rtsp_backend` / `--rtsp-backend` (CLI, then config, then GStreamer if built in, else native; an unbuilt backend is a startup error); `rtsp` and `mqtt-rtsp` with either backend; all four feature combinations build and test |
| 4.2 | `src/rtsp_native`:<br>- `NativeSource` maps `StreamEvent` → `SourceUnit` (ADPCM dropped, warned once; Paused/Retrying states);<br>- mounts come from the 15 s passive stream-info poll, seeded for sleeping cameras;<br>- descs are cached in the state dir |
| 4.3 | Hot reload:<br>- users and `rtsp_auth` swap the server's auth;<br>- `permitted_users` are read per request;<br>- adding or removing a camera, or a change that shapes its streams, recreates its Hubs (BYE);<br>- entries are tagged per generation;<br>- a camera whose task ends is restarted |
| 4.4 | New keys from DESIGN §7, validated and in the unknown-key lists. A reload adding `certificate` or `tls_client_auth` is refused whole, before any camera sees it |
| 4.5 | `mqtt-rtsp` runs on the native backend:<br>- `mqtt_config_writes` gates the config topic, with a startup warning when users exist;<br>- `config/status` no longer echoes a parse error's source line |
| 4.6 | R28: info lines for connect, close, play and stop (IP, mount, user), auth failures, and source start/stop; the Hub stats line at debug every 60 s |
| 4.7 | `src/rtsp_native/tests.rs` against `FakeCamera`:<br>- one wake per DESCRIBE+PLAY, released after linger + grace;<br>- A21 after a restart (0 contacts over 22 s of probes);<br>- A12: 60 s idle, past `bridge_max_s`, then motion;<br>- hot reload: users, a refused certificate, removal and re-add;<br>- A8: 30 cycles back to the task baseline;<br>- main+sub together;<br>- a taken snapshot port;<br>- shutdown BYE;<br>- battery unknown is treated as battery |

**Gate 4 (CI part).** The Gate 3 suite was re-run through `neolink rtsp` (native) and
`xtask fakecam` (`SERVER=neolink` in `tests/e2e/{clients,golden-native,avsync}.sh`; CI runs
both paths):
- A1 and A2 at 120 s (0 decode errors), A2b, A4, A4b (strip), A5 at 300 s (0 lost, 0 too late);
- A10: 21 of 21;
- A19: 28 of 28;
- A16: worst 73 ms at 15 min.

Defects this found, all fixed:
- the H.265 EOS packet;
- re-latching by 1 µs (duplicate DTS);
- a late joiner's cached IRAP stamped at a stale wire time;
- audio SRs delayed by up to 2 s (VLC played audio 2–8 s late);
- a learning start continuing as Demand without honouring `pause.on_motion`;
- in the fake camera: VIDEO_STOP stopped all streams, and loop timing was wrong.

**Red-team review.** 11 findings, all fixed:
- the snapshot port taking the server down;
- a half-applied refused reload;
- shutdown order (R27);
- linger (P17);
- learning limits per camera, persisted, applied only when starting costs a wake, and one
  wake teaches every kind (P18);
- a camera not restarting;
- a teardown/re-add race;
- the `rtp_mtu` reload;
- a false Retrying state at start;
- a test gated for no-backend builds;
- weak tests strengthened.

**Notes for users:**
- `pause.on_motion` needs motion events, which a camera sends only over an open connection.
  On a sleeping camera, use push notifications (`pushnoti`), or the camera must stay
  connected.
- `bind`, `bind_port`, `limits` and `rtcp_sr` apply at start. `rtcp_sr` is per server: it
  stays on while any camera wants it.
- R11's per-client-IP learning limit is covered by the stricter per-camera limit.

**Pending (hardware, ≤ 2 sessions/day, battery ≥ 40 %):**
- the four Gate 4 sessions;
- battery deltas;
- the Phase 1 deferred 10-cycle check.


## Phase 5: client matrix and hardening (8–12 d; 2–3 hardware sessions, more with mains)

| # | Task |
|---|---|
| 5.1 | Frigate's preset ffmpeg command lines: A3 in CI (15 min, crossing the wrap) and nightly (90 min). On hardware: 1 session of 10 min. A real Frigate container is optional and manual |
| 5.2 | `bridge_fps` tuning with Frigate's preset command lines and go2rtc (A12's tolerance criterion) |
| 5.3 | VLC and retina timeline checks on hardware (1 session) |
| 5.4 | ~~Blue Iris (A14)~~: dropped by owner decision |
| 5.5 | Nightly fuzz job (30 min per target); exact-timing nightly job on a dedicated runner |
| 5.6 | Soak: 24 h on fixtures (A15 surrogate, nightly). 24 h hardware only with mains power; otherwise deferred |
| 5.7 | Performance (N3, N4) on a Pi 4 **if available**, otherwise any aarch64 SBC/VM, with fixtures. Decryption cost is measured separately with a camera-layer benchmark on the scripted peer, using AES on the replay |

**Gate 5**
- Every A-test in spec §7 passes, or has a recorded owner-approved deferral.
- No open P1 compatibility bug.
- Red-team review of the full branch.

## Phase 6: packaging and beta release (4–6 d; 1 hardware session)

| # | Task |
|---|---|
| 6.1 | `cross` musl builds for x86_64 and aarch64 (jemalloc page size); `--no-default-features --features native-rtsp` |
| 6.2 | `FROM scratch` image `:native` (root default, `PUID`/`PGID`, `/state`, CA certs) and `:native-debug` (busybox, `/entrypoint.sh` shim) |
| 6.3 | CI container test (A13, N9). The container job uses the scripted peer as a fake camera (from 1.2a, exposed as a tiny `neolink-fakecam` test binary) |
| 6.4 | Third-party license notices in artifacts (`cargo-about`). Provenance headers on files ported from bairelay: bairelay files carry no per-file headers, so ours say `Portions adapted from bairelay (AGPL-3.0-or-later), <commit>` |
| 6.5 | Docs: README native section, migration notes, release notes for every spec §5 delta, Docker changes (no shell, root default, `PUID`), the `irap.bin` privacy note, the MQTT trust boundary, the bridging storage note |

**Gate 6**
- A13 and N9 pass in CI.
- **Fresh-machine test** (1 hardware session): download the binary, write a config, run
  it, connect Frigate's preset command line. No other install steps.

**Status (2026-10-11), Phases 5 and 6, fixture and CI parts** (`feature/native-rtsp-p5`):
- **5.1 A3:** Frigate 0.14's preset-rtsp-generic (record segments + 5 fps detect) and
  preset-rtsp-restream for 15 min across the u32 wrap: no reconnect, 0 errors, 90 of 90
  segments (`tests/e2e/frigate.sh`). One ffmpeg startup note is tolerated: under
  `-use_wallclock_as_timestamps`, "Non-monotonic DTS; previous: 0, current: 0". The
  GStreamer backend doesn't trigger it; it should be re-checked on hardware.
- **5.2:** a 60 s stall at `bridge_fps` 0.2, with and without audio, doesn't trip Frigate's
  5 s `-timeout`.
- **5.5:** nightly workflow: fuzz for 30 min per target, A3 and A16 for 90 min, soak 5 h.
  The 24 h soak needs a self-hosted runner (`SOAK_SECS=86400`).
- **5.6:** `tests/e2e/soak.sh` (neolink + scripted camera, two clients, RSS ≤ +10 %).
- **5.7:** N3: 1.98 % of one core for 4K H.265 (3.3 Mb/s) to 4 clients; N4: 58 KiB per added
  client (x86_64). No Pi or arm64 host is available.
- **6.1:** static musl builds:
  - x86_64 static-PIE, 14 MB; runs in a scratch image;
  - aarch64 static, 11.6 MB, with jemalloc 64 KiB pages; built, not run (no arm64 host).
- **6.2:** `Dockerfile.native` (`FROM scratch`, 14.3 MB, root by default, `PUID`/`PGID`, state
  volume, CA certs).
- **6.3:** `tests/e2e/container-native.sh`: A13 plays; `PUID` drops; N9: `podman stop` with a
  client playing takes 297 ms, exit 0, one logout. In CI.
- **6.4:** `cargo-about` notice (76 entries). No bairelay code was ported, so no provenance headers are needed.
  Re-checked 2026-10-12 against bairelay 6787e47: 95 of 3,572 significant lines in
  `crates/rtsp/src` match a bairelay line (derive lists, `use` lines, test attributes, the
  ISO 14496-3 AAC sample-rate table) and no run of 4 consecutive lines matches.
- **6.5:** `docs/native-rtsp/NATIVE.md` covers install, config keys, battery behaviour, the
  §5 deltas, Docker changes, the `irap.bin` privacy note, the MQTT trust boundary and
  bridged media. The README links to it.
- **Gate 5 full-branch red-team review:** 11 findings, all fixed:
  - **Docs and builds:**
    - push notifications are compiled out of the native build. They would also break
      local-only networking, so NATIVE.md now says plainly that `pause.on_motion` can't
      start a sleeping camera, and neolink warns at startup;
    - hot reload is via MQTT only, and the docs now say so;
    - the snapshot endpoint and the remaining keys are documented;
    - the Frigate claim is corrected;
    - the leftover env vars are removed.
  - **Security:**
    - an unreadable MQTT CA is now an error, not a fall back to plaintext;
    - an MQTT config write that points a camera or the broker elsewhere no longer
      receives the current secrets;
    - the snapshot endpoint paces failed logins.
  - **Power:**
    - with `PUID`, the state directory is handed to that user first and checked for
      writes;
    - the learning gate shares one wake for 30 s, so main and sub opened together both
      succeed. The 1 h soak caught this: sub got 503 on a cold start.
  - **Tests:**
    - `perf.sh` N4 uses a fresh server;
    - the soak also asserts one camera login and no neolink errors.
  - The soak (600 s after the fix; 1 h before it, main only) passes with 1 login, no
    errors and RSS ±4 %.
- **Pending (hardware):** see `docs/native-rtsp/HARDWARE-SESSIONS.md`:
  - Gate 4's four sessions;
  - 5.1 and 5.3;
  - the Phase 1 10-cycle check;
  - the Gate 6 fresh-machine test.

## Phase 7: default flip (later, spec §8)

Not scheduled.

## Totals

| Phase | Focused days | Hardware sessions |
|---|---|---|
| 0 | 4–6 | 2 |
| 1 | 16–22 | 2 |
| 2 | 12–16 | 0 |
| 3 | 10–14 | 0 |
| 4 | 6–9 | 3–4 |
| 5 | 8–12 | 2–3 |
| 6 | 4–6 | 1 |
| Review/fix buffer (7 gates, about 12 %) | 6–9 | — |
| **Total** | **66–94 d** | **10–12** |

For one developer that is about 3–4 months of calendar time. Phases run strictly in order
(0 → 1 → … → 6). There is one developer, and Phase 2 depends on Gate 0's answers (Q2 shapes
the MediaClock; 0.7 provides fixtures). v1's "Phase 2 in parallel with Phase 1" is withdrawn.

## Risk register

| Risk | L | I | Mitigation | Trigger |
|---|---|---|---|---|
| Client incompatibility found late (Blue Iris, XProtect, Synology) | M | H | Golden structural diff; client CI image from Phase 3; Phase 5 matrix | Any P1 compatibility bug |
| Pinned ffmpeg autobuild deleted by BtbN before the first remote CI run | H | M | ✅ Mirrored, unmodified and with its corresponding source, on the `mirror-ffmpeg-n8.1.3-17` prerelease; `.github/actions/setup-ffmpeg` downloads it from there (sha256-checked). Fixtures regenerate only with the same build | Closed 2026-10-12 |
| Clients that don't retry SETUP over TCP after 461 (Synology, XProtect defaults unverified) | M | H | Test them in Phase 5; UDP is first in the §4.2 backlog | A confirmed client failure → pull UDP into v1 |
| Battery drain from testing, or from design bugs (waking on probes) | M | M | A21; hardware budget; `battery: None` fail-safe | Battery delta > 5 % per session |
| A/V offset wrong because the arrival-skew model is wrong | M | M | 0.4 measurement before 2.4; A16 at 5 s / 60 s / 15 min / 90 min | A16 fails |
| Camera behaviour differs on other models (NAL 62, ADPCM, Info frames, mains bursts) | H | M | Whitelist, explicit ADPCM drop, fixture variants; ask beta users for `bcmedia_dump` captures. No mains/wired camera available, so the 300 ms pacing default is unvalidated | Beta reports |
| **Distribution**: upstream inactive since January 2025, the only remote is upstream, no fork/registry/release channel | H | H | Owner decision 4 before Gate 1; set up a fork remote, GHCR and a release workflow in Phase 6 | No decision by Gate 1 → Phase 1 can't ship |
| **Opportunity cost of not adopting bairelay**: 58–84 d to reach a subset of bairelay 1.1.2; its fixes won't flow into ported code | H | M | The owner made this decision knowingly (spec §3). Re-check at Gate 3: if the estimate overruns by > 30 %, re-present the adopt/contribute option | Gate 3 overrun |
| AGPL compliance of ported code and static artifacts | L | M | 6.4 provenance headers and cargo-about notices | — |
| Bus factor: one developer and a single reviewer pool | H | M | Docs-first (these three files); every gate red-teamed by a fresh agent; owner sign-off | — |
| Flaky real-time tests in CI | M | M | Virtual time in-process; correctness-only assertions with external clients; exact timing nightly on a dedicated runner | > 2 % flake rate |
| ffmpeg/VLC/go2rtc version variance | M | M | Pinned versions in the client image (0.8, 3.9) | — |
| `retina` MSRV/version (0.4.13 has an H.265 profile/tier/level bug) | M | L | Separate `rtsp-it` crate on the newest retina, excluded from MSRV | — |
| Scope creep (UDP, TLS) during v1 | M | M | Spec §4.2 order; deferrals recorded here | — |
| Hidden coupling in neolink (permits, reactor drop semantics) | M | H | Phase 1 first; scripted-peer tests; Gate 1 per-cycle check | Gate 1 failures |
| Fork divergence from other neolink forks and from bairelay | H | L | Keep changes in new modules; minimise edits to shared files | — |

## Owner decisions (2026-10-10)

0. **Use case: on-demand viewing** of a battery/solar camera (HA dashboard, VLC). Power management is first-class (spec §4.1.1) and lands in Phase 1, before any RTSP work. Motion-triggered wake is v1.x.

1. **Fixture permission: yes.** Short captures of the Argus may be committed as CI fixtures.
   0.5's `argus_reconnect.bcm` and a trimmed capture from 0.4 go under
   `crates/rtsp/tests/fixtures/`. Synthetic fixtures are still used for wrap, reset and
   A/V-sync tests.
2. **Mains power: no** (solar only). A15 runs on fixtures only. The hardware budget stays at ≤ 10 min sessions, ≥ 40 % battery, and daytime sessions where possible.
3. **Windows / Blue Iris: out of scope.** A14 is dropped. Windows artifacts stay out of
   v1.x (spec §4.2 item 4 is reduced to armv7 and macOS).
4. **Pi 4: none.** N3/N4 run on an aarch64 VM or CI runner with fixtures. The Pi 4 numbers
   in spec §6 become "aarch64 reference runner" targets.
5. **Distribution: a personal fork.** No upstream PRs. Phase 6 targets the owner's fork:
   GitHub releases plus GHCR images. Until the fork exists, work stays on local branches.
