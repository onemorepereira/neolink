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
| 0.1 | ✅ Capture tool `crates/core/examples/bcmedia_dump.rs` | Done, uncommitted |
| 0.2 | ✅ Argus characterisation (spec §9) | Done |
| 0.3 | ✅ Raw golden transcripts: open + auth, main + sub, TCP + default (UDP) | Raw, in scratchpad |
| 0.3b | **Golden tooling**: a recording TCP proxy (test utility), a normaliser, and a **structural** diff. The diff compares methods, status codes, header presence and shape, SDP attribute sets and fmtp keys. It does not compare values that depend on the source: sprop, ssrc, seq, rtptime, dates, resolution. One hardware session re-records all 19 aliases plus extern-404, open and auth, TCP and UDP, with ffmpeg pinned (see 0.8). | `tests/golden/*.txt` committed (normalised, no camera imagery) |
| 0.4 | 10 min main capture (1 session). Measure the **arrival-skew distribution** (sign and spread) and the AU-vs-cam_us drift, estimated as the **slope of the arrival offset over time**; AU counting alone has 107 ppm resolution. Confirms the median estimator (DESIGN §4.2) | Skew histogram, drift estimate with a confidence interval, initial `skew_estimate` |
| 0.5 | Wake/reconnect: let the camera sleep, then reconnect (the same session as 0.4, using `idle_disconnect`). Does `cam_us` reset? | Fixture `argus_reconnect.bcm` (kept locally unless 0.9 allows committing) |
| 0.6 | Silent AAC-LC AU, mono and stereo: generate with ffmpeg, verify that ffmpeg and symphonia decode it to zeros | Bytes plus a unit test |
| 0.7 | `xtask mkfixture` (Rust): ffmpeg lavfi testsrc2 + sine with a periodic flash/beep → Annex-B/ADTS → `.bcm` with the Argus burst schedule. Variants: wrap (start at 2³²−5 min), reset, outage, resolution change | Reproducible CI fixtures; no owner footage |
| 0.8 | CI: `test.yml` (`cargo test --workspace`); tokio ≥ 1.39 lock bump; `rust-version = "1.82"`; **pin a static ffmpeg build** (BtbN, version recorded) for every ffmpeg-based test, because request shapes and error strings differ between ffmpeg 6 and 8 | CI green on the current tree |
| 0.9 | Owner decisions (below): fixture permission, mains power, Windows/Blue Iris, Pi 4, distribution | Recorded here |
| 0.10 | Confirm the NFR targets (spec §6) from 0.4 and the golden data; adjust the spec if they're unrealistic | Spec diff |

**Gate 0**
- Spec §9 Q1, Q2 and Q5 are answered.
- Fixtures are generated reproducibly in CI.
- `cargo test --workspace` runs in CI.
- The golden tooling diffs a re-recorded GStreamer transcript against itself: 0 structural
  deltas.

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

**Gate 1**
- `podman stop` < 5 s with exit 0 on the GStreamer image (N9).
- **100 connect/teardown cycles against the scripted peer in CI**: the use-counter returns
  to 0 after *each* cycle, and the task count returns to baseline.
- On hardware, 10 cycles with a 6 s gap (1 session): the use-counter returns to 0 each
  cycle, checked with the 1.10 dump.
- **A23–A26 and A28–A30 pass against the scripted peer**; the peer mocks the p2p cloud host and counts broadcasts.
- **A27 on hardware (1 evening session).**
- The golden structural diff against the GStreamer backend is clean.
- No password appears in `RUST_LOG=trace` output (grep test).
- The config corpus parses.
- Red-team review of the Phase 1 diff.

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
