//! HTTP route handlers for the camera API.
//!
//! Stubs: filled in by Tasks 7-12. Each stub returns the correct type but
//! `unimplemented!()` at runtime.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde_json::json;
use std::sync::Arc;

use crate::api::server::AppState;
use crate::api::model::{CameraRequest, CameraResponse};
use crate::config::Config;

/// GET /cameras — list all cameras
pub(crate) async fn get_cameras(
    _state: State<Arc<AppState>>,
) -> Result<Json<Vec<CameraResponse>>, StatusCode> {
    unimplemented!("Task 7: GET /cameras")
}

/// POST /cameras — create a camera
pub(crate) async fn post_camera(
    _state: State<Arc<AppState>>,
    _body: Json<CameraRequest>,
) -> Result<(StatusCode, Json<CameraResponse>), StatusCode> {
    unimplemented!("Task 8: POST /cameras")
}

/// GET /cameras/:name — get one camera
pub(crate) async fn get_camera(
    _state: State<Arc<AppState>>,
    _name: Path<String>,
) -> Result<Json<CameraResponse>, StatusCode> {
    unimplemented!("Task 7: GET /cameras/:name")
}

/// PUT /cameras/:name — update a camera (partial)
pub(crate) async fn put_camera(
    _state: State<Arc<AppState>>,
    _name: Path<String>,
    _body: Json<CameraRequest>,
) -> Result<Json<CameraResponse>, StatusCode> {
    unimplemented!("Task 8: PUT /cameras/:name")
}

/// DELETE /cameras/:name — remove a camera
pub(crate) async fn delete_camera(
    _state: State<Arc<AppState>>,
    _name: Path<String>,
) -> Result<StatusCode, StatusCode> {
    unimplemented!("Task 8: DELETE /cameras/:name")
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
