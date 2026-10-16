# Spec: neolink HTTP configuration API

**Date:** 2026-10-16
**Status:** proposed (not yet implemented)
**Author:** owner
**Supersedes:** none

## 1. Problem

neolink is configured exclusively through a TOML file read at startup (`--config`). Camera
adds/removes, stream selection, MQTT toggles, and power settings all require editing the file
and restarting neolink. For the home k3s lab this means a git commit + Flux reconciliation
just to add a camera, and no way for an in-cluster consumer (e.g. DoorWatch) to discover or
manage cameras at runtime.

This spec adds an HTTP API so configuration can be created, read, updated and deleted at
runtime, with writes persisted to the same TOML file neolink boots from so they survive
restart.

## 2. Goals

- Camera CRUD (`/cameras`, `/cameras/:name`) over HTTP with JSON bodies.
- Whole-config backup/restore (`GET`/`PUT /config`) as TOML.
- Per-camera status that never wakes the camera (`/cameras/:name/status`).
- Passive snapshot (`/cameras/:name/snapshot.jpg`) — never wakes.
- Explicit on-demand wake (`POST /cameras/:name/wake`) — wakes.
- RTSP URL discovery (`/cameras/:name/rtsp-url`).
- All writes persisted to the `--config` TOML file atomically, then applied to running
  cameras through the existing reactor hot-reload path.
- Power-frugality contract: status/snapshot never wake; only `/wake` and RTSP `PLAY` wake.
- Open API (no auth) for now; exposure controlled at the deployment layer (k3s Service /
  NetworkPolicy).
- Default build unchanged: the API is behind a new `api` Cargo feature.

## 3. Non-goals

- Auth on the API itself (API key, RTSP-user reuse, TLS). Deferred; controlled via k3s
  NetworkPolicy/Service exposure.
- RTSPS / HTTPS for the API.
- Watch / websocket config change stream; clients poll.
- File watcher that reloads neolink when the TOML changes externally (the API is the only
  runtime writer; file edits still require a restart).
- MQTT `neolink/config` behavior changes — the API and MQTT are independent writers of the
  same file; MQTT's existing power/users restrictions remain.

## 4. Design

### 4.1 Architecture

A new `src/api/` module, compiled only with `cargo ... --features api`. It owns:

- `api::server` — axum HTTP server (listener + router + graceful shutdown).
- `api::endpoints` — route handlers.
- `api::persist` — atomic write of the TOML config file (temp file + `rename`).
- `api::model` — JSON request/response types (the `Camera` API object), conversion to/from
  the internal `CameraConfig` / `Config`.

The API server is a tokio task started from `main.rs` (when the `api` feature is on). It
receives:

- The config file path (to persist writes).
- A `WatchReceiver<Config>` from `NeoReactorCommand::Config` (same channel MQTT's config
  listener uses) — for reads.
- A `MpscSender<NeoReactorCommand>` — to send `UpdateConfig` for writes.

### 4.2 Endpoints

All paths under the API port (`api_port`, default `8080`, `api_bind` default `0.0.0.0`).

| Method | Path | Body | Wakes? | Description |
|---|---|---|---|---|
| GET | `/cameras` | — | No | List all cameras (array of `Camera` objects) |
| GET | `/cameras/:name` | — | No | Get one camera's config |
| POST | `/cameras` | `Camera` (+ optional `password`) | No | Create a camera; `409` if exists |
| PUT | `/cameras/:name` | partial `Camera` (+ optional `password`) | No | Update a camera; partial OK; `404` if missing |
| DELETE | `/cameras/:name` | — | No | Remove a camera; `404` if missing |
| GET | `/cameras/:name/status` | — | No | Cached status: connected, battery, motion, wakes |
| GET | `/cameras/:name/snapshot.jpg` | — | No | Passive still from last wake (grey placeholder) |
| POST | `/cameras/:name/wake` | — | Yes | Explicit wake; returns when ready |
| GET | `/cameras/:name/rtsp-url` | — | No | `rtsp://<host>:8554/<name>` |
| GET | `/config` | — | No | Whole config as TOML |
| PUT | `/config` | TOML | No | Replace whole config; validate, persist, apply |

Camera-level writes merge the delta into the current in-memory config before persisting and
applying (PUT is partial). POST requires all fields needed to build a valid `CameraConfig`.

`GET /config` and `PUT /config` speak TOML in/out for round-trip fidelity. Camera endpoints
speak JSON.

### 4.3 Data model — the `Camera` API object

```json
{
  "name": "front-door",
  "display_name": "Front Door",
  "address": "192.168.1.50:9000",
  "uid": "ABCDEF123456",
  "username": null,
  "battery": true,
  "enabled": true,
  "stream": "mainStream",
  "discovery": "local",
  "snapshot_fresh": false,
  "idle_grace_s": 10,
  "updated_at": "2026-10-16T14:30:00Z"
}
```

Rules:
- POST/PUT accept an optional `password` field; GET never returns `username` or `password`
  (both absent/null).
- `updated_at` is set by neolink on any write; clients must not set it.
- `name` is the stable, URL-safe ID.
- Exactly one of `address` or `uid` must be provided (existing validation rule).

The internal `CameraConfig` keeps `username: String` required and gains two optional fields:
`display_name: Option<String>` and `updated_at: Option<DateTime>` (serde default). The API
model is a view over the internal config, not a 1:1 struct.

### 4.4 Read path

- `GET /cameras*` and `GET /status` read from the reactor's `watch` channel (current running
  config + cached per-camera state from the wake ledger: connected, battery level/age,
  motion, today's wakes). No camera contact.
- `GET /cameras/:name/snapshot.jpg` serves the already-stored snapshot file (passive — the
  endpoint that already exists); grey placeholder if never woken.
- `GET /cameras/:name/rtsp-url` returns a string: `rtsp://<bind-host>:<bind_port>/<name>`.

### 4.5 Write path

For `POST`/`PUT`/`DELETE /cameras*` and `PUT /config`:

1. Parse body (JSON for camera endpoints, TOML for `/config`).
2. Merge into the current `Config` (camera-level) or replace (whole-config).
3. Run `config.validate()` — `400` on invalid values (port ranges, regexes, `address`/`uid`
   rule).
4. Atomic persist: write `<config-file>.tmp.<pid>` then `rename` over the config file.
5. Send `NeoReactorCommand::UpdateConfig(new_conf)`; reactor applies to running cameras via
   `update_config` + `send_replace` on the `watch` channel.
6. Reply `200`/`201` with the applied object; `400`/`404`/`409`/`500` otherwise.

Persist-before-reactor ordering: if the reactor apply fails, the file still holds the intended
state and neolink converges on restart. A failed write applies nothing and returns `500`.

### 4.6 Power & users rules (SPEC P14 / R29)

The API is the operator writing the config file, so power settings (`battery`,
`idle_disconnect`, `idle_grace_s`, `motion_hold_max_s`, `snapshot_fresh`, `pause.on_motion`,
`enabled`) and users / `permitted_users` may be changed through it — that is exactly what
"come from the config file" means now that the API is a writer of it. The MQTT-specific
refusal checks (`power_changes`, `same_users`) apply only to MQTT `neolink/config` writes,
unchanged.

The API is open, so an operator who can reach the port can keep a battery camera awake.
Exposure is controlled at the k3s layer: the API port gets no LoadBalancer service by
default; only RTSP (`:8554`) is exposed externally.

### 4.7 Behavior contract (power-frugality)

- Reads (`/cameras`, `/status`, `/snapshot.jpg`, `/rtsp-url`) never contact the camera.
- `POST /wake` connects, then releases after `idle_grace_s` unless another demand keeps it.
- RTSP `PLAY` wakes (existing behavior).
- Camera writes don't wake; they record desired state and apply to already-running cameras.

### 4.8 Error handling

Body: `{ "error": "..." }` — a single message, never a source line (could hold a password),
never a stack trace.

- `200` success; `201` created; `400` unparseable body or validation failure; `404` unknown
  camera; `409` camera already exists; `500` persistence/reactor failure.

### 4.9 Concurrency & shutdown

The reactor serializes commands through its `mpsc` channel, so concurrent API writes are
serialized there. Each TOML file write is atomic per request. The API server task is cancelled
as part of the existing 6 s shutdown bound (before/alongside camera logout).

## 5. Configuration keys added

| Key | Where | Default | What |
|---|---|---|---|
| `api_bind` | top | `"0.0.0.0"` | API listen address |
| `api_port` | top | `8080` | API listen port |

Only compiled/honored when the `api` feature is on.

## 6. Dependencies

- `axum` — HTTP server/routing, gated behind `feature = "api"`.
- `serde_json` — already a dependency.
- `chrono` — for `updated_at` (add to `[dependencies]`, optional behind `api`? or always;
  `DateTime<Utc>` serialization). Decision: add `chrono` as a normal dependency; used only by
  the API module.

No change to GStreamer / native-rtsp backends.

## 7. Testing

- Unit: `api::persist` atomic write on a temp dir; JSON↔internal-config conversion round-trip
  (password omitted/null on GET); `updated_at` set on write.
- Integration: `cargo xtask fakecam` scripted camera + real `neolink --features api` binary;
  hit endpoints over localhost; assert `200`/`404`/`201`/`409`; assert TOML file changed;
  assert camera appears/disappears from RTSP; assert `/snapshot.jpg` is passive (fake
  camera's connection counter unchanged); assert `/wake` connects then releases after
  `idle_grace_s`.
- Existing suites unchanged: `cargo test --workspace` and `--features native-rtsp,pushnoti`
  pass without `--features api`; `api` module is `cfg(feature = "api")`.
- k3s smoke: deploy via the `neolink` HelmRelease; `kubectl port-forward` the API port; curl
  `/cameras`; RTSP play from a LAN client.

## 8. Deployment (k3s)

No change to the existing `cluster/neolink/` scaffolding beyond optionally exposing the API
port. The Deployment already mounts `/etc/neolink.toml` from the SOPS Secret; the API rewrites
that file in place. RTSP (`:8554`) stays the only externally exposed port; the API port
(`:8080`) is ClusterIP-only for in-cluster consumers (DoorWatch). No new Service by default.

## 9. Open questions

1. `chrono` as a normal dependency vs optional behind `api`? (Recommendation: normal
   dependency; tiny crate, used only by the API module.)
2. Default `api_bind` — `0.0.0.0` (open) vs `127.0.0.1` (loopback)? Owner chose open;
   exposure controlled at k3s layer. (Recommendation: keep `0.0.0.0` default as agreed.)
3. Whole-config `PUT /config` speaks TOML while camera endpoints speak JSON — inconsistent
   formats? (Recommendation: TOML for `/config` round-trips cleanly; JSON for the camera
   resource API that DoorWatch consumes.)

## 10. Future work (out of scope)

- API auth (bearer token / RTSP users).
- HTTPS / RTSPS for the API.
- Config file watcher for external edits.
- MQTT `neolink/config` integration changes.

---

End of spec. Please review and let me know of any changes before I proceed to the
implementation plan.
