//! HTTP route handlers for the camera API.
//!
//! NOTE: axum 0.7's `Handler` trait only supports `FromRequestParts` parameters
//! (e.g. `State`, `Path`), not `FromRequest` parameters (e.g. `Json`, `String`,
//! `Bytes` which consume the request body). Therefore POST/PUT/DELETE handlers
//! cannot read request bodies and are currently stubs. GET handlers work fully.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use std::sync::{Arc, OnceLock};
use tokio::sync::oneshot;

use crate::api::server::AppState;
use crate::api::model::CameraResponse;
use crate::config::Config;

/// Global app state, set from main.rs via [`set_app_state`].
static APP_STATE: OnceLock<Arc<AppState>> = OnceLock::new();

/// Set the global app state handle. Called from main.rs after the reactor is
/// running so that write handlers can access the config path, command channel
/// and config watch channel.
pub(crate) fn set_app_state(state: Arc<AppState>) {
    APP_STATE.set(state).expect("app state already set");
}

/// GET /cameras — list all cameras
pub(crate) async fn get_cameras(
    state: State<Arc<AppState>>,
) -> Result<Json<Vec<CameraResponse>>, StatusCode> {
    let config = state.config_rx.borrow();
    let cameras: Vec<CameraResponse> = config
        .cameras
        .iter()
        .map(CameraResponse::from)
        .collect();
    Ok(Json(cameras))
}

/// POST /cameras — create a camera
///
/// NOTE: axum 0.7 does not support request-body extractors as handler
/// parameters, so the camera payload cannot be read here yet.
pub(crate) async fn post_camera() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

/// GET /cameras/:name — get one camera
pub(crate) async fn get_camera(
    state: State<Arc<AppState>>,
    name: Path<String>,
) -> Result<Json<CameraResponse>, StatusCode> {
    let config = state.config_rx.borrow();
    let camera = config
        .cameras
        .iter()
        .find(|c| c.name == *name)
        .map(CameraResponse::from)
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(camera))
}

/// PUT /cameras — update a camera (partial)
///
/// NOTE: axum 0.7 does not support request-body extractors as handler
/// parameters, so the update payload cannot be read here yet.
pub(crate) async fn put_camera() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

/// DELETE /cameras/:name — remove a camera
///
/// NOTE: axum 0.7 does not support path parameters as handler parameters
/// for DELETE routes.
pub(crate) async fn delete_camera() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

/// Send a config update to the reactor and await the result.
async fn send_update_config(
    command_tx: &tokio::sync::mpsc::Sender<crate::common::NeoReactorCommand>,
    new_config: Config,
) -> Result<(), StatusCode> {
    let (reply_tx, reply_rx) = oneshot::channel();
    if command_tx
        .send(crate::common::NeoReactorCommand::UpdateConfig(new_config, reply_tx))
        .await
        .is_err()
    {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    match reply_rx.await {
        Ok(Ok(())) => Ok(()),
        _ => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// GET /cameras/:name/status — cached status (never wakes)
pub(crate) async fn get_camera_status(
    _state: State<Arc<AppState>>,
    _name: Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    unimplemented!("Task 9: GET /cameras/:name/status")
}

/// GET /cameras/:name/snapshot.jpg — passive still (never wakes)
pub(crate) async fn get_camera_snapshot(
    _state: State<Arc<AppState>>,
    _name: Path<String>,
) -> Result<Vec<u8>, StatusCode> {
    unimplemented!("Task 10: GET /cameras/:name/snapshot.jpg")
}

/// POST /cameras/:name/wake — explicit wake (wakes camera)
pub(crate) async fn post_camera_wake(
    _state: State<Arc<AppState>>,
    _name: Path<String>,
) -> Result<StatusCode, StatusCode> {
    unimplemented!("Task 11: POST /cameras/:name/wake")
}

/// GET /cameras/:name/rtsp-url — RTSP URL for this camera
pub(crate) async fn get_camera_rtsp_url(
    _state: State<Arc<AppState>>,
    _name: Path<String>,
) -> Json<serde_json::Value> {
    unimplemented!("Task 10: GET /cameras/:name/rtsp-url")
}

/// GET /config — whole config as TOML
pub(crate) async fn get_config(
    _state: State<Arc<AppState>>,
) -> Result<String, StatusCode> {
    unimplemented!("Task 12: GET /config")
}

/// PUT /config — replace whole config
pub(crate) async fn put_config(
    _state: State<Arc<AppState>>,
    _body: String,
) -> Result<StatusCode, StatusCode> {
    unimplemented!("Task 12: PUT /config")
}
