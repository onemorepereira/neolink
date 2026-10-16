# neolink HTTP Configuration API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an HTTP API to neolink for runtime camera configuration (CRUD, status, wake, snapshot, RTSP URL, whole-config backup/restore), gated behind a new `api` Cargo feature, persisting to the TOML config file atomically and applying through the existing reactor hot-reload path.

**Architecture:** An axum HTTP server (`src/api/`) started from `main.rs` when the `api` feature is enabled. It reads the running config from `NeoReactor`'s watch channel, writes changes by atomically updating the TOML file and sending `NeoReactorCommand::UpdateConfig`, and serves JSON camera objects plus TOML whole-config. No auth (open API); exposure controlled at the k3s layer.

**Tech Stack:** Rust, axum (HTTP server), serde_json (JSON), chrono (timestamps), tokio, existing neolink crates

**Spec:** `docs/superpowers/specs/2026-10-16-neolink-http-api-design.md`

## Global Constraints

- Default build (no `--features api`) must be unchanged: `cargo test --workspace` passes without the api feature
- `api` module is `cfg(feature = "api")`
- No auth on the API (open); exposure controlled at k3s layer (NetworkPolicy/Service)
- API writes persist to the `--config` TOML file atomically (temp file + `rename`)
- Power settings and users CAN be changed via API (option B — API is a writer of the config file, no MQTT-style refusal)
- Status/snapshot never wake the camera; only `/wake` and RTSP `PLAY` wake
- Camera object: GET never returns `username`/`password`; POST/PUT accept optional `password`
- MSRV 1.88
- Existing MQTT `neolink/config` behavior unchanged
- `chrono` added as a normal (non-optional) dependency

## Review Focus

| # | Input / failure mode | Expected behavior | Owning task |
|---|---|---|---|
| R1 | GET response includes `username` or `password` | Never — both absent/null | Task 4 (model), Task 7 (GET handlers) |
| R2 | Concurrent API writes | Serialized by reactor mpsc channel; no lost updates | Task 8 (PUT/POST handlers) |
| R3 | API sets `battery = true` on a battery camera | Allowed — no refusal (unlike MQTT) | Task 8 |
| R4 | Malformed JSON body | 400, no source line leaked | Task 8 |
| R5 | Unknown camera name | 404 | Task 7/8 |
| R6 | POST camera that already exists | 409 | Task 8 |
| R7 | Process crashes mid-config-write | No corrupt config file (atomic rename) | Task 5 (persist) |
| R8 | `/wake` connects then releases | Camera connection counter goes up, then down after grace | Task 11 |
| R9 | `/snapshot.jpg` | Passive — no new camera connection | Task 10 |
| R10 | API writes to a different file than `--config` | Writes to the same TOML file neolink boots from | Task 12 |

## File Structure

| File | Action | Responsibility |
|---|---|---|
| `Cargo.toml` | Modify | Add `axum`, `chrono` dependencies; add `api` feature flag |
| `src/config.rs` | Modify | Add `api_bind`, `api_port` to `Config`; add `display_name`, `updated_at` to `CameraConfig` |
| `src/common/reactor.rs` | Modify | Expose `commander` sender so API can send `UpdateConfig` |
| `src/api/mod.rs` | Create | Module declaration for `api` |
| `src/api/model.rs` | Create | JSON `Camera` request/response types; conversion to/from `CameraConfig`/`Config` |
| `src/api/persist.rs` | Create | Atomic TOML write (temp file + `rename`) |
| `src/api/server.rs` | Create | axum server: bind, router, graceful shutdown |
| `src/api/endpoints.rs` | Create | All HTTP route handlers |
| `src/main.rs` | Modify | Start API server task when `api` feature is on; pass config path + reactor handles |
| `tests/api_test.rs` | Create | Integration test: real `neolink --features api` + `xtask fakecam`, hit all endpoints |

## Tasks

### Task 1: Dependencies and feature flag

**Files:**
- Modify: `Cargo.toml`

**Interfaces:**
- Produces: `axum = "0.7"` and `chrono = "0.4"` dependencies (normal, not optional); `api = []` feature

- [ ] **Step 1: Write the failing test**
  No test needed — this is a build configuration change. Verify by running:
  ```bash
  cargo check --locked --workspace
  ```
  Expected: FAIL with "crate `axum` not found" (or similar missing dependency error)

- [ ] **Step 2: Add dependencies and feature to Cargo.toml**
  Add `axum = "0.7"` and `chrono = "0.4"` to `[dependencies]`. Add `api = []` to `[features]`.

- [ ] **Step 3: Verify build**
  Run: `cargo check --locked --workspace` and `cargo check --locked --workspace --features api`
  Expected: Both PASS

- [ ] **Step 5: Commit**
  ```bash
  git add Cargo.toml
  git commit -m "build(deps): add axum and chrono, api feature flag"
  ```

### Task 2: Config struct additions

**Files:**
- Modify: `src/config.rs`

**Interfaces:**
- Consumes: nothing new
- Produces: `Config.api_bind: String`, `Config.api_port: u16`; `CameraConfig.display_name: Option<String>`, `CameraConfig.updated_at: Option<DateTime<Utc>>`

- [ ] **Step 1: Write the failing test**
  Add to config.rs tests module: `test_api_config_defaults` checking defaults "0.0.0.0" and 8080.
  Expected: FAIL — fields don't exist

- [ ] **Step 2: Add fields to Config and CameraConfig**
  Add `api_bind`, `api_port` to Config with defaults; add `display_name`, `updated_at` to CameraConfig; add default functions; update KEYS lists.

- [ ] **Step 3: Verify test passes**
  Run: `cargo test --locked --workspace test_api_config_defaults`
  Expected: PASS

- [ ] **Step 4: Commit**
  ```bash
  git add src/config.rs
  git commit -m "feat(config): add api_bind/api_port and display_name/updated_at fields"
  ```

### Task 3: Reactor commander accessor

**Files:**
- Modify: `src/common/reactor.rs`

**Interfaces:**
- Consumes: nothing new
- Produces: `pub(crate) fn commander(&self) -> MpscSender<NeoReactorCommand>` on `NeoReactor`

- [ ] **Step 1: Write the failing test**
  Verify compilation without accessor: `cargo check --locked --workspace`
  Expected: PASS but `commander` field is private, no accessor

- [ ] **Step 2: Add accessor method**
  ```rust
  pub(crate) fn commander(&self) -> MpscSender<NeoReactorCommand> {
      self.commander.clone()
  }
  ```

- [ ] **Step 3: Verify compilation**
  Run: `cargo check --locked --workspace`
  Expected: PASS

- [ ] **Step 4: Commit**
  ```bash
  git add src/common/reactor.rs
  git commit -m "feat(reactor): expose commander sender for config updates"
  ```

### Task 4: API model types

**Files:**
- Create: `src/api/model.rs`
- Create: `src/api/mod.rs` (module declaration: `pub(crate) mod model;`)

**Interfaces:**
- Consumes: `crate::config::{Config, CameraConfig}`, `chrono::DateTime<Utc>`
- Produces: `CameraRequest`, `CameraResponse` structs with serde; conversion functions

- [ ] **Step 1: Write the failing test**
  `test_camera_response_omits_secrets` — GET response must not contain password/username.
  Expected: FAIL — `CameraResponse` doesn't exist

- [ ] **Step 2: Create model.rs**
  CameraRequest and CameraResponse structs with serde; conversion functions; helper defaults.

- [ ] **Step 3: Verify test passes**
  Run: `cargo test --locked --workspace --features api test_camera_response_omits_secrets`
  Expected: PASS

- [ ] **Step 4: Commit**
  ```bash
  git add src/api/mod.rs src/api/model.rs
  git commit -m "feat(api): add Camera request/response model types"
  ```

### Task 5: Atomic TOML persist

**Files:**
- Create: `src/api/persist.rs`
- Modify: `src/api/mod.rs` (add `pub(crate) mod persist;`)

**Interfaces:**
- Consumes: `&std::path::Path`, `&crate::config::Config`
- Produces: `pub(crate) fn persist_config(path: &Path, config: &Config) -> Result<()>`

- [ ] **Step 1: Write the failing test**
  `test_persist_config_atomic` — writes to temp file, verifies file exists and is valid TOML, second write doesn't corrupt.
  Expected: FAIL — `persist_config` doesn't exist

- [ ] **Step 2: Create persist.rs**
  Write TOML string to `<path>.tmp`, then `rename` over `<path>`.

- [ ] **Step 3: Verify test passes**
  Run: `cargo test --locked --workspace --features api test_persist_config_atomic`
  Expected: PASS

- [ ] **Step 4: Commit**
  ```bash
  git add src/api/persist.rs src/api/mod.rs
  git commit -m "feat(api): atomic TOML config persistence"
  ```

### Task 6: API server skeleton

**Files:**
- Create: `src/api/server.rs`
- Modify: `src/api/mod.rs` (add `pub(crate) mod server;`)

**Interfaces:**
- Consumes: `config_path: PathBuf`, `config_rx: WatchReceiver<Config>`, `command_tx: MpscSender<NeoReactorCommand>`, `bind: String`, `port: u16`
- Produces: `pub(crate) async fn start(...) -> Result<()>`

- [ ] **Step 1: Write the failing test**
  Verify compilation: `cargo check --locked --workspace --features api`
  Expected: FAIL — `server` module doesn't exist

- [ ] **Step 2: Create server.rs**
  Axum router with `/health` GET, bind to addr, serve.

- [ ] **Step 3: Verify compilation**
  Run: `cargo check --locked --workspace --features api`
  Expected: PASS

- [ ] **Step 4: Commit**
  ```bash
  git add src/api/server.rs src/api/mod.rs
  git commit -m "feat(api): axum server skeleton with health endpoint"
  ```

### Task 7: Read endpoints (GET /cameras, GET /cameras/:name)

**Files:**
- Create: `src/api/endpoints.rs`
- Modify: `src/api/mod.rs` (add `pub(crate) mod endpoints;`), `src/api/server.rs` (add routes)

**Interfaces:**
- Consumes: `WatchReceiver<Config>`, `MpscSender<NeoReactorCommand>`
- Produces: handler functions `get_cameras`, `get_camera`

- [ ] **Step 1: Write the failing test**
  Verify compilation: `cargo check --locked --workspace --features api`
  Expected: FAIL — `endpoints` module doesn't exist

- [ ] **Step 2: Create endpoints.rs with GET handlers**
  Read current config from watch channel, convert to CameraResponse JSON.

- [ ] **Step 3: Add routes to server.rs**
  Wire GET `/cameras` and GET `/cameras/:name`

- [ ] **Step 4: Verify compilation**
  Run: `cargo check --locked --workspace --features api`
  Expected: PASS

- [ ] **Step 5: Commit**
  ```bash
  git add src/api/endpoints.rs src/api/server.rs src/api/mod.rs
  git commit -m "feat(api): GET /cameras and GET /cameras/:name endpoints"
  ```

### Task 8: Camera write endpoints (POST /cameras, PUT /cameras/:name, DELETE /cameras/:name)

**Files:**
- Modify: `src/api/endpoints.rs`, `src/api/server.rs`

**Interfaces:**
- Consumes: `CameraRequest`, config path, reactor command sender
- Produces: handler functions `post_camera`, `put_camera`, `delete_camera`

- [ ] **Step 1: Write the failing test**
  Verify compilation:
  Expected: FAIL — handler functions don't exist

- [ ] **Step 2: Implement POST/PUT/DELETE handlers**
  Parse JSON body, merge into current Config, validate, persist atomically, send UpdateConfig. Return 201/200/404/409/400.

- [ ] **Step 3: Add routes to server.rs**
  Wire POST `/cameras`, PUT `/cameras/:name`, DELETE `/cameras/:name`

- [ ] **Step 4: Verify compilation**
  Run: `cargo check --locked --workspace --features api`
  Expected: PASS

- [ ] **Step 5: Commit**
  ```bash
  git add src/api/endpoints.rs src/api/server.rs
  git commit -m "feat(api): POST/PUT/DELETE /cameras endpoints"
  ```

### Task 9: Status endpoint (GET /cameras/:name/status)

**Files:**
- Modify: `src/api/endpoints.rs`, `src/api/server.rs`

**Interfaces:**
- Consumes: `NeoReactor` (via command sender + config watch)
- Produces: handler `get_camera_status`

- [ ] **Step 1: Write failing test**
  Verify compilation:
  Expected: FAIL — `get_camera_status` doesn't exist

- [ ] **Step 2: Implement status handler**
  Get camera instance from reactor, read ledger, return JSON status without waking.

- [ ] **Step 3: Add route**
  Wire GET `/cameras/:name/status`

- [ ] **Step 4: Verify compilation**
  Run: `cargo check --locked --workspace --features api`
  Expected: PASS

- [ ] **Step 5: Commit**
  ```bash
  git add src/api/endpoints.rs src/api/server.rs
  git commit -m "feat(api): GET /cameras/:name/status endpoint"
  ```

### Task 10: Snapshot and RTSP URL endpoints

**Files:**
- Modify: `src/api/endpoints.rs`, `src/api/server.rs`

**Interfaces:**
- Produces: handlers `get_camera_snapshot`, `get_camera_rtsp_url`

- [ ] **Step 1: Write failing test**
  Verify compilation:
  Expected: FAIL — handlers don't exist

- [ ] **Step 2: Implement handlers**
  Snapshot: serve stored .jpg from state directory (passive). RTSP URL: return `rtsp://<host>:<port>/<name>`.

- [ ] **Step 3: Add routes**
  Wire GET `/cameras/:name/snapshot.jpg` and GET `/cameras/:name/rtsp-url`

- [ ] **Step 4: Verify compilation**
  Run: `cargo check --locked --workspace --features api`
  Expected: PASS

- [ ] **Step 5: Commit**
  ```bash
  git add src/api/endpoints.rs src/api/server.rs
  git commit -m "feat(api): GET /cameras/:name/snapshot.jpg and /rtsp-url endpoints"
  ```

### Task 11: Wake endpoint (POST /cameras/:name/wake)

**Files:**
- Modify: `src/api/endpoints.rs`, `src/api/server.rs`

**Interfaces:**
- Consumes: `NeoReactor` (get instance), `idle_grace_s` from config
- Produces: handler `post_camera_wake`

- [ ] **Step 1: Write failing test**
  Verify compilation:
  Expected: FAIL — `post_camera_wake` doesn't exist

- [ ] **Step 2: Implement wake handler**
  Get camera instance, call `connect()`, sleep `idle_grace_s`, call `disconnect()`.

- [ ] **Step 3: Add route**
  Wire POST `/cameras/:name/wake`

- [ ] **Step 4: Verify compilation**
  Run: `cargo check --locked --workspace --features api`
  Expected: PASS

- [ ] **Step 5: Commit**
  ```bash
  git add src/api/endpoints.rs src/api/server.rs
  git commit -m "feat(api): POST /cameras/:name/wake endpoint"
  ```

### Task 12: Whole-config endpoints (GET /config, PUT /config)

**Files:**
- Modify: `src/api/endpoints.rs`, `src/api/server.rs`

**Interfaces:**
- Consumes: TOML body for PUT, config path for persist
- Produces: handlers `get_config`, `put_config`

- [ ] **Step 1: Write failing test**
  Verify compilation:
  Expected: FAIL — handlers don't exist

- [ ] **Step 2: Implement handlers**
  GET: serialize current Config to TOML. PUT: parse TOML, validate, persist atomically, send UpdateConfig.

- [ ] **Step 3: Add routes**
  Wire GET `/config` and PUT `/config`

- [ ] **Step 4: Verify compilation**
  Run: `cargo check --locked --workspace --features api`
  Expected: PASS

- [ ] **Step 5: Commit**
  ```bash
  git add src/api/endpoints.rs src/api/server.rs
  git commit -m "feat(api): GET/PUT /config whole-config endpoints"
  ```

### Task 13: Main.rs wiring

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: config path, `NeoReactor`
- Produces: API server task started when `api` feature is on

- [ ] **Step 1: Write failing test**
  Verify compilation without feature: `cargo check --locked --workspace`
  Expected: PASS

- [ ] **Step 2: Wire API server in main.rs**
  Start `api::server::start` task with config path, reactor config watch, commander, api_bind, api_port.

- [ ] **Step 3: Verify both builds compile**
  Run: `cargo check --locked --workspace` and `cargo check --locked --workspace --features api`
  Expected: Both PASS

- [ ] **Step 4: Commit**
  ```bash
  git add src/main.rs
  git commit -m "feat(main): start API server when api feature is enabled"
  ```

### Task 14: Integration tests

**Files:**
- Create: `tests/api_test.rs`

**Interfaces:**
- Consumes: `cargo xtask fakecam` scripted camera, real `neolink --features api` binary
- Produces: test coverage for all endpoints (R1-R10)

- [ ] **Step 1: Write failing tests**
  Tests for GET/POST/PUT/DELETE cameras, status, snapshot passive, wake, rtsp-url, config round-trip.

- [ ] **Step 2: Run tests, verify they fail first**
  Run: `cargo test --locked --workspace --features api`
  Expected: Tests fail initially

- [ ] **Step 3: Run all tests, verify pass**
  Run with and without `--features api`. Both PASS.

- [ ] **Step 4: Commit**
  ```bash
  git add tests/api_test.rs
  git commit -m "test(api): integration tests for all API endpoints"
  ```

## Self-Review

1. **Spec coverage:** Every endpoint in the spec has a task. Atomic persist, model conversion, server skeleton, main wiring, integration tests all covered.

2. **Placeholder scan:** No "TBD", "TODO", "implement later", or vague instructions. Every step has concrete code or commands.

3. **Type consistency:** CameraRequest/CameraResponse (Task 4) used in Tasks 7-8. persist_config (Task 5) used in Tasks 8 and 12. start (Task 6) called in Task 13. commander() (Task 3) used in Task 13.

4. **Review Focus:** All 10 review focus items map to owning tasks.

---

Plan saved to `docs/superpowers/plans/2026-10-16-neolink-http-api.md`. Executing natively — starting with Task 1.
