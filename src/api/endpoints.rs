//! HTTP route handlers for the camera API.

use axum::{
    extract::{Path, State, Json},
    http::{StatusCode, header},
    response::Response,
};
use std::sync::Arc;
use tokio::sync::oneshot;
use validator::Validate;

use crate::api::persist;
use crate::api::server::AppState;
use crate::api::model::{CameraRequest, CameraResponse};
use crate::common::{state_dir, StateFile};
use crate::config::Config;

// ── GET /cameras ──────────────────────────────────────────────────────────────

pub(crate) async fn get_cameras(
    State(st): State<Arc<AppState>>,
) -> Result<Json<Vec<CameraResponse>>, StatusCode> {
    let cameras = st.config_rx.borrow().cameras.iter().map(CameraResponse::from).collect();
    Ok(Json(cameras))
}

// ── POST /cameras ─────────────────────────────────────────────────────────────

pub(crate) async fn post_camera(
    State(st): State<Arc<AppState>>,
    Json(req): Json<CameraRequest>,
) -> Result<(StatusCode, Json<CameraResponse>), StatusCode> {
    let config = st.config_rx.borrow().clone();
    if config.cameras.iter().any(|c| c.name == req.name) {
        return Err(StatusCode::CONFLICT);
    }
    let new_cam = req.resolve_for_create(None).map_err(|_| StatusCode::BAD_REQUEST)?;

    let mut new_config = config;
    new_config.cameras.push(new_cam);
    new_config.validate().map_err(|_| StatusCode::BAD_REQUEST)?;

    persist::persist_config(&st.config_path, &new_config)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    update_config(&st.command_tx, new_config).await?;

    let camera = st.config_rx.borrow().cameras.iter()
        .find(|c| c.name == req.name)
        .map(CameraResponse::from)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((StatusCode::CREATED, Json(camera)))
}

// ── GET /cameras/:name ────────────────────────────────────────────────────────

pub(crate) async fn get_camera(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<CameraResponse>, StatusCode> {
    st.config_rx.borrow().cameras.iter()
        .find(|c| c.name == name)
        .map(|c| Json(CameraResponse::from(c)))
        .ok_or(StatusCode::NOT_FOUND)
}

// ── PUT /cameras/:name ────────────────────────────────────────────────────────

pub(crate) async fn put_camera(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<CameraRequest>,
) -> Result<Json<CameraResponse>, StatusCode> {
    let config = st.config_rx.borrow().clone();
    let idx = config.cameras.iter().position(|c| c.name == name)
        .ok_or(StatusCode::NOT_FOUND)?;

    let mut new_config = config;
    req.apply_to(&mut new_config.cameras[idx]);
    new_config.validate().map_err(|_| StatusCode::BAD_REQUEST)?;

    persist::persist_config(&st.config_path, &new_config)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    update_config(&st.command_tx, new_config).await?;

    let camera = st.config_rx.borrow().cameras.iter()
        .find(|c| c.name == name)
        .map(|c| Json(CameraResponse::from(c)))
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(camera)
}

// ── DELETE /cameras/:name ─────────────────────────────────────────────────────

pub(crate) async fn delete_camera(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let config = st.config_rx.borrow().clone();
    if !config.cameras.iter().any(|c| c.name == name) {
        return Err(StatusCode::NOT_FOUND);
    }
    let mut new_config = config;
    new_config.cameras.retain(|c| c.name != name);
    new_config.validate().map_err(|_| StatusCode::BAD_REQUEST)?;

    persist::persist_config(&st.config_path, &new_config)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    update_config(&st.command_tx, new_config).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── GET /cameras/:name/status ─────────────────────────────────────────────────

pub(crate) async fn get_camera_status(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let cam_config = st.config_rx.borrow().cameras.iter()
        .find(|c| c.name == name)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;

    // Read cached state from disk — never wakes the camera
    let sf = StateFile::for_camera(&cam_config);
    let state = sf.load();

    let battery_level = state.last_battery.as_ref().map(|b| b.percent);
    let battery_age_s = state.last_battery.as_ref().map(|b| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|now| now.as_secs().saturating_sub(b.at))
            .unwrap_or(0)
    });

    Ok(Json(serde_json::json!({
        "name":          name,
        "connected":     false,      // cached; live value only while awake
        "battery_level": battery_level,
        "battery_age_s": battery_age_s,
        "motion":        "unknown",  // only known over an open connection
        "today_wakes":   0,          // ledger not yet wired through API
    })))
}

// ── GET /cameras/:name/snapshot.jpg ──────────────────────────────────────────
// 1×1 grey JPEG placeholder (when no snapshot has been saved yet)
static GREY_JPEG: &[u8] = include_bytes!("snapshot_placeholder.jpg");

pub(crate) async fn get_camera_snapshot(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Response<axum::body::Body>, StatusCode> {
    let cam_config = st.config_rx.borrow().cameras.iter()
        .find(|c| c.name == name)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;

    let sf = StateFile::for_camera(&cam_config);
    let jpeg = match sf.snapshot() {
        Some((bytes, _at)) => bytes,
        None => GREY_JPEG.to_vec(),
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .body(axum::body::Body::from(jpeg))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

// ── POST /cameras/:name/wake ──────────────────────────────────────────────────

pub(crate) async fn post_camera_wake(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let cam_config = st.config_rx.borrow().cameras.iter()
        .find(|c| c.name == name)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;

    let instance = st.reactor.get(&name).await.map_err(|_| StatusCode::NOT_FOUND)?;
    instance.connect().await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    // Release after idle_grace_s so the caller can connect via RTSP
    let grace = cam_config.idle_grace_s;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(grace)).await;
        let _ = instance.disconnect().await;
    });

    Ok(StatusCode::NO_CONTENT)
}

// ── GET /cameras/:name/rtsp-url ───────────────────────────────────────────────

pub(crate) async fn get_camera_rtsp_url(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let config = st.config_rx.borrow();
    if !config.cameras.iter().any(|c| c.name == name) {
        return Err(StatusCode::NOT_FOUND);
    }
    let url = format!("rtsp://{}:{}/{}", config.bind_addr, config.bind_port, name);
    Ok(Json(serde_json::json!({ "rtsp_url": url })))
}

// ── GET /config ───────────────────────────────────────────────────────────────

pub(crate) async fn get_config(
    State(st): State<Arc<AppState>>,
) -> Result<String, StatusCode> {
    toml::to_string(&*st.config_rx.borrow())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

// ── PUT /config ───────────────────────────────────────────────────────────────

pub(crate) async fn put_config(
    State(st): State<Arc<AppState>>,
    body: String,
) -> Result<StatusCode, StatusCode> {
    crate::config::warn_unknown_keys(&body, "PUT /config");
    let mut config: Config = toml::from_str(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    config.validate().map_err(|_| StatusCode::BAD_REQUEST)?;

    persist::persist_config(&st.config_path, &config)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    update_config(&st.command_tx, config).await?;
    Ok(StatusCode::OK)
}

// ── internal ──────────────────────────────────────────────────────────────────

async fn update_config(
    tx: &tokio::sync::mpsc::Sender<crate::common::NeoReactorCommand>,
    config: Config,
) -> Result<(), StatusCode> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(crate::common::NeoReactorCommand::UpdateConfig(config, reply_tx))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    reply_rx.await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
