//! HTTP configuration API (feature `api`).
//!
//! Camera CRUD, status, wake, snapshot, RTSP URL and whole-config backup/restore.
//! The API is open (no auth); exposure is controlled at the deployment layer (k3s
//! Service / NetworkPolicy). Writes are persisted atomically to the `--config` TOML
//! file and applied to running cameras through `NeoReactor::UpdateConfig`.

pub(crate) mod model;
pub(crate) mod persist;
pub(crate) mod server;
pub(crate) mod endpoints;
