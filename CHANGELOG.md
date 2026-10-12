# Changelog

Notable changes to this fork of neolink. The baseline is upstream commit `6e05e78`.

## Unreleased

### BREAKING: cameras sleep by default

Before, neolink connected to every camera at startup and stayed connected, unless the
camera set `idle_disconnect = true`. Now **a camera with neither `battery` nor
`idle_disconnect` in its config sleeps**:
- neolink does not contact it at startup;
- it connects only while something uses the camera: an RTSP client, an MQTT
  `control/*` message or a `query/*` with the payload `fresh`, a CLI command, or a
  `snapshot.jpg?fresh=1`;
- it logs out and disconnects `idle_grace_s` seconds (default 10) after the last use.

This applies to both RTSP backends (GStreamer and native) and to every mode. It is the
right behaviour for battery and solar cameras, but it also applies to mains, PoE and NVR
cameras until you mark them.

**What stops working for a sleeping camera, until something wakes it:**
- MQTT `status/motion` (stays `unknown`): the camera sends motion events only over an
  open connection;
- `pause.on_motion` can't start the stream (neolink warns at startup);
- MQTT `status/floodlight` and `status/floodlight_tasks`;
- MQTT `status/preview` and `status/battery_level` are published once per wake instead of
  every `preview_update`/`battery_update` ms;
- `neolink/<camera>/status` is `disconnected` while the camera sleeps.

**Upgrade steps:**
1. Add `battery = false` to every mains, PoE and NVR-channel `[[cameras]]` entry. It then
   stays connected as before.
2. Leave battery and solar cameras as they are (or add `battery = true` to silence the
   startup info line).
3. For a battery camera that must stay connected (24/7 recording, `pause.on_motion`, MQTT
   motion), set `idle_disconnect = false`. It drains the battery, and neolink warns.
4. In Docker, mount a volume on `/var/lib/neolink` (the state directory).
5. Check the startup log: each camera that sleeps only because neither key is set gets
   an info line: ``<camera>: connects only while in use (battery or unknown power); set
   `battery = false` to keep a mains camera connected as before``.

The rules: `idle_disconnect`, when set, decides (`true` sleeps, `false` stays connected);
otherwise the camera sleeps unless `battery = false`. Neolink never detects mains power by
itself. See [README: Power](README.md#power-sleeping-and-always-on-cameras).

### Other behaviour changes

- **Release delay.** A sleeping camera is released `idle_grace_s` (default 10 s) after its
  last use, instead of a fixed 30 s, and the startup connection no longer lingers until a
  first use.
- **Motion.** With `pause.on_motion`, a motion event keeps the camera connected for at
  most `motion_hold_max_s` (default 120 s). Motion state resets to unknown on disconnect.
- **Push notifications** (`pushnoti` builds only) keep the camera connected for 30 s only
  when `pause.on_motion` is set or the camera is always-on.
- **Discovery.** For UID cameras, local discovery runs first and the Reolink cloud methods
  only if it fails, never in parallel (a cloud registration can wake a battery camera).
  When the cloud is allowed, local discovery gives up after 8 s, so a cloud-only camera
  connects up to 8 s later than before.
- **Reconnects** back off from 1 s to 60 s while there is demand, and every 5 min after
  10 min offline. A sleeping camera with no demand is not retried.
- **MQTT and Home Assistant.**
  - Discovery entities' availability now follows `neolink/status` (neolink itself)
    instead of `neolink/<camera>/status`, and each camera gets a new **Awake** binary
    sensor driven by `neolink/<camera>/status`.
  - Retained `neolink/config`, `control/*` and `query/*` messages are ignored with a
    warning, so a broker reconnect doesn't replay them.
  - New top-level `mqtt_config_writes` (default `true`): set `false` to stop MQTT clients
    rewriting the config. Neolink warns at startup when it is on and users are
    configured.
- **MQTT on sleeping cameras.**
  - `control/wakeup <minutes>` is capped at 30 minutes (larger values are capped,
    non-numbers get `FAIL:`), a new wakeup replaces the pending one, `0` cancels it, and
    it ends when neolink stops or the camera is removed. Before, any value was accepted
    and wakeups stacked, so a camera could be held awake indefinitely.
  - `query/battery` and `query/preview` answer from the last wake (the reply gives its
    age); `query/pir` and `query/ptz/preset` answer only while the camera is awake. The
    payload `fresh` wakes the camera; concurrent `fresh` queries share one wake.
  - `status/motion` is published `off` when the camera sleeps or drops mid-motion,
    instead of staying `on`.
  - MQTT wakes are recorded as `mqtt:<topic>` in the wake ledger.
  - **Power settings come only from the config file.** A config published to
    `neolink/config` that changes a camera's `battery`, `idle_disconnect` (`idle`,
    `idle_disc`), `idle_grace_s`, `motion_hold_max_s`, `snapshot_fresh` or
    `pause.on_motion`, or that enables a camera the file disables, is refused whole with
    `FAIL: ...` on `neolink/config/status` naming the cameras and keys, and a warning in
    the log. Leaving out a key the file sets counts as a change. A camera added over MQTT
    must leave those keys unset, so it sleeps. Before, any MQTT client could set
    `battery = false` or `idle_disconnect = false` and keep a battery camera awake. Other
    config writes (such as `[cameras.mqtt]` toggles) work as before.
  - **Users and permissions come only from the config file, too.** A config write that
    changes `[[users]]` (adds, removes or re-passwords one) or any camera's
    `permitted_users` is refused whole with `FAIL: ...`. Before, an MQTT client could add
    itself as a user or open a camera to `anonymous`.
- **CLI `battery` and `image`** print what was saved at the camera's last wake, without
  waking it; `--fresh` wakes the camera (`image --use-stream` always does).
- **Config.** Unknown keys (for example a misspelt `idle_disconect`) are reported with a
  warning instead of being silently ignored. Secrets are redacted from config debug
  output.
- **Docker.** See [Docker](#docker) below.
- **Shutdown.** SIGTERM and SIGINT log the cameras out before exiting.

### Docker

- **neolink is the image's entrypoint and PID 1.** `docker stop` now logs the cameras out
  and exits 0 within a second; before, the shell entrypoint swallowed SIGTERM and the
  container was killed after 10 s. The command may be neolink's arguments (new:
  `--config ...`, `rtsp --config ...`) or, as before, `neolink ...` or any other program
  (`sh -c "..."`, a script), which neolink runs in its place. A neolink subcommand wins
  over a program of the same name (`users`). `/entrypoint.sh` remains for setups that
  name it. In a `sh -c` command, end with `exec neolink ...` so that the stop reaches
  neolink.
- **`NEO_LINK_MODE` applies whenever the command has no subcommand**, including
  `neolink --config ...`; before, that form always ran `rtsp`. An option value that
  happens to be a subcommand name (`--config rtsp`) is no longer taken for one.
- **State directory.** `/var/lib/neolink` (`NEOLINK_STATE_DIR`) is a volume. With
  `PUID`/`PGID`, neolink hands the state directory and the root-owned files it wrote
  there as root (`camera/`, `cache/`, `learn-*`) to that user, never following symlinks
  and never the filesystem root. At startup it checks that the directory and `camera/`
  are writable and logs an error otherwise (also with `--user`).
- neolink stops at startup if it cannot install its SIGTERM/SIGINT handlers, instead of
  running deaf to `docker stop`.
- The kubernetes example ends its `sh -c` with `exec`.
- `tests/e2e/container-entry.sh` runs these cases against a built image.
- **Native image base.** `Dockerfile.native` takes its CA certificates from Alpine 3.24.2
  (was 3.20, end of life) and builds with Rust 1.91.0 on Alpine 3.22 (was 1.90.0 on 3.20);
  the native release binaries build with Rust 1.91.0 too.

### Added

- **Snapshot endpoint** (both backends, `rtsp` and `mqtt-rtsp` modes):
  `http://<bind>:<bind_port + 1>/<camera>/snapshot.jpg` serves the picture taken at the
  camera's last wake and never wakes it (a placeholder until the first wake). `?fresh=1`
  wakes the camera, only with the per-camera `snapshot_fresh = true`, at most once a
  minute. Auth follows `[[users]]` and `permitted_users` (HTTP Basic). **It listens on a
  new port by default**: set `snapshot_port = 0` to turn it off, or another port if
  `bind_port + 1` is taken (8555 is also go2rtc's WebRTC port).
- **State directory** (`$NEOLINK_STATE_DIR`, else `$XDG_STATE_HOME/neolink`, else
  `~/.local/state/neolink`, else `/tmp/neolink`): per camera, what neolink learnt at its
  last wake (UID, model, firmware, streams, battery reading, snapshot). A lock file warns
  when a second neolink process uses the same camera.
- **Wake ledger**: an info line each time a sleeping camera is released, with why it was
  awake and the day's totals; published retained on MQTT as
  `neolink/<camera>/status/wakes` (JSON); a warning when a camera wakes more than 12 times
  in an hour.
- **New camera keys**: `battery`, `idle_grace_s`, `motion_hold_max_s`, `snapshot_fresh`.
  `idle_disconnect` is unchanged in meaning but now defaults to "follow `battery`".
- A warning when a sleeping camera has streamed for 30 min straight.
- **Native RTSP backend (beta)**, behind the `native-rtsp` Cargo feature (not in the
  default features): an RTSP server without GStreamer, built as a static binary
  (`cargo build --release --no-default-features --features native-rtsp`) and a
  `FROM scratch` image (`Dockerfile.native`). Select it with `rtsp_backend = "native"` or
  `--rtsp-backend native`; GStreamer stays the default where it is built in. It answers
  `DESCRIBE` from a cache so health probes don't wake a sleeping camera, and adds the
  keys `rtsp_backend`, `rtsp_auth`, `rtp_mtu`, `limits.*`, `audio`, `pacing_latency_ms`,
  `parameter_sets`, `bridge_fps`, `bridge_max_s`, `rtcp_sr` and `persist_irap`. It has no
  RTSPS and no push notifications yet. See
  [docs/native-rtsp/NATIVE.md](docs/native-rtsp/NATIVE.md) and
  [docs/native-rtsp/STATUS.md](docs/native-rtsp/STATUS.md).

### Fixed

- PIR control on newer firmwares; a malformed XML reply no longer drops the connection.
- MQTT with an unreachable broker: `mqtt` and `mqtt-rtsp` no longer exit (status 1) when
  the broker is down at startup or goes away. Neolink logs a warning and retries with
  backoff (1 s doubling to 60 s); what it publishes meanwhile waits and is sent once the
  broker is back. SIGTERM/SIGINT still stops it promptly.
- MQTT retained topics stay retained: after a reconnection (including to a broker that
  restarted without persistence) neolink publishes its retained topics (camera status,
  discovery, config, previews, ...) again, retained. Before, a retained publish retried
  after a disconnect went out without the retain flag.
- A camera's MQTT listener that fails is restarted with backoff (1 s doubling to 60 s)
  instead of in a tight loop. A restart contacts no camera, so it never wakes a sleeping
  one.
- MQTT no longer warns "Ignoring retained MQTT message on neolink/config" on every
  connection: that message was neolink's own retained config. Someone else's retained
  config still gets the warning.
- The version string comes only from release tags (`v*`); without one it is the crate
  version and the commit, such as `0.6.3-rc.3 (b15ad3c)`.
- The camera's time is logged with the right UTC offset sign (a camera on UTC-6 showed
  `+06:00`). The offset is the camera's time zone without daylight saving.
- Native RTSP: a client that hangs up, as ffmpeg does right after TEARDOWN, is logged at
  debug level and no longer as "write failed or stalled". A real stall is logged as
  "client took no data for ...".
- Native RTSP: stream timestamps no longer drift away from real time after network
  congestion. Before, each congestion episode that the stream bridged (likely at the
  mains default `pacing_latency_ms` of 300) moved the RTP and RTCP sender-report times
  about 8 s further into the future, without limit, and catching up after an episode made
  an unlogged jump of several seconds. Now each catch-up, resume or camera clock reset
  brings the timestamps back in line with real time at a keyframe, and every such step is logged
  (info when over 1 s). The stats line also counts the pacer's pull-backs and skips and
  the timestamp rebases. Tested with ffmpeg, go2rtc, Frigate's command lines and VLC: all
  stay connected and recordings stay valid; VLC freezes for several seconds around a
  congestion episode (up to ~14 s while it re-buffers late video).

### Security

- Dependency updates for RustSec advisories: openssl, h2, rustls, ring, bytes,
  crossbeam-channel, anyhow, rand and url/idna (lock file); quick-xml 0.41, rumqttc 0.25
  and validator 0.20 (manifest). MQTT over TLS now uses rustls 0.23 with the ring
  provider. Advisories with no fix usable here (time needs Rust 1.88; the `pushnoti`
  feature's fcm-push-listener 2 TLS/HTTP stack; rumqttc's leftover rustls-webpki 0.102)
  are ignored in `deny.toml`, each with the condition for removing it.
