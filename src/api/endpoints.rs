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
use crate::common::StateFile;
use crate::config::Config;

/// Return a 4xx with a plain-text body so the request log captures the reason.
fn err(status: StatusCode, msg: impl std::fmt::Display) -> StatusCode {
    // We log the message here; the middleware logs the response body on 4xx.
    // Since handlers return StatusCode (no body), log directly.
    log::warn!("api error {}: {}", status.as_u16(), msg);
    status
}

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
    let new_cam = req.resolve_for_create(None).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;

    let mut new_config = config;
    new_config.cameras.push(new_cam);
    new_config.validate().map_err(|e| err(StatusCode::BAD_REQUEST, e))?;

    persist::persist_config(&st.config_path, &new_config).await
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
    new_config.validate().map_err(|e| err(StatusCode::BAD_REQUEST, e))?;

    persist::persist_config(&st.config_path, &new_config).await
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
    new_config.validate().map_err(|e| err(StatusCode::BAD_REQUEST, e))?;

    persist::persist_config(&st.config_path, &new_config).await
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

    // ── cached state from disk (never wakes) ──────────────────────────────────
    let sf = StateFile::for_camera(&cam_config);
    let disk = sf.load();

    // ── live data from reactor (never wakes) ──────────────────────────────────
    let (ledger_status, motion_str, connected, active_users) = match st.reactor.get(&name).await {
        Err(_) => (None, "unknown".to_string(), false, 0u32),
        Ok(instance) => {
            // Ledger: wake history and awake state
            let ledger_status = instance.wakes().await.ok()
                .map(|rx| rx.borrow().clone());

            // Motion: current state from the motion watcher
            let motion_str = match instance.motion().await.ok()
                .map(|rx| rx.borrow().clone())
            {
                Some(crate::common::MdState::Start(_)) => "on",
                Some(crate::common::MdState::Stop(_))  => "off",
                _                                       => "unknown",
            }.to_string();

            // Connected = awake flag from ledger
            let connected = ledger_status.as_ref().map(|l| l.awake).unwrap_or(false);

            // Active users = permit count (streams, MQTT commands, etc.)
            let active_users = st.reactor.get(&name).await
                .map(|_| 0u32)  // use_count is on NeoCam not NeoInstance
                .unwrap_or(0);

            (ledger_status, motion_str, connected, active_users)
        }
    };

    // ── assemble response ──────────────────────────────────────────────────────
    let battery = disk.last_battery.as_ref().map(|b| serde_json::json!({
        "percent":        b.percent,
        "voltage_mv":     b.voltage,
        "current_ma":     b.current,
        "temperature_c":  b.temperature,
        "charge_status":  b.charge_status,
        "adapter_status": b.adapter_status,
        "age_s":          b.age(),
    }));

    let wakes = ledger_status.as_ref().map(|l| serde_json::json!({
        "awake":           l.awake,
        "reason":          l.reason,
        "since":           l.since,
        "last_awake_s":    l.last_awake_s,
        "today_wakes":     l.today_wakes,
        "today_awake_s":   l.today_awake_s,
        "last_hour_wakes": l.last_hour_wakes,
    }));

    Ok(Json(serde_json::json!({
        "name":               name,
        "connected":          connected,
        "motion":             motion_str,
        "battery":            battery,
        "wakes":              wakes,
        "snapshot_at":        disk.snapshot_at,
        "supported_streams":  disk.supported_streams,
        "model":              disk.model,
        "firmware":           disk.firmware,
        "uid":                disk.uid,
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
    let mut config: Config = toml::from_str(&body).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    config.validate().map_err(|e| err(StatusCode::BAD_REQUEST, e))?;

    persist::persist_config(&st.config_path, &config).await
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
