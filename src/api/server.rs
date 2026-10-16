//! axum HTTP server: bind, router, graceful shutdown.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc::Sender as MpscSender, watch::Receiver as WatchReceiver};
use tokio_util::sync::CancellationToken;

use anyhow::Result;
use axum::{
    Router,
    routing::{get, post, put, delete},
    http::StatusCode,
    extract::State,
};
use crate::config::Config;
use crate::common::{NeoReactorCommand, NeoReactor};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) config_path: PathBuf,
    pub(crate) config_rx:   WatchReceiver<Config>,
    pub(crate) command_tx:  MpscSender<NeoReactorCommand>,
    pub(crate) reactor:     NeoReactor,
}

pub(crate) async fn start(
    config_path: PathBuf,
    config_rx:   WatchReceiver<Config>,
    command_tx:  MpscSender<NeoReactorCommand>,
    reactor:     NeoReactor,
    bind:        String,
    port:        u16,
    cancel:      CancellationToken,
) -> Result<()> {
    let state = Arc::new(AppState { config_path, config_rx, command_tx, reactor });

    let app = Router::new()
        .route("/health", get(health))
        .route(
            "/cameras",
            get(crate::api::endpoints::get_cameras)
                .post(crate::api::endpoints::post_camera),
        )
        .route(
            "/cameras/:name",
            get(crate::api::endpoints::get_camera)
                .put(crate::api::endpoints::put_camera)
                .delete(crate::api::endpoints::delete_camera),
        )
        .route("/cameras/:name/status",       get(crate::api::endpoints::get_camera_status))
        .route("/cameras/:name/snapshot.jpg", get(crate::api::endpoints::get_camera_snapshot))
        .route("/cameras/:name/wake",         post(crate::api::endpoints::post_camera_wake))
        .route("/cameras/:name/rtsp-url",     get(crate::api::endpoints::get_camera_rtsp_url))
        .route(
            "/config",
            get(crate::api::endpoints::get_config)
                .put(crate::api::endpoints::put_config),
        )
        .with_state(state);

    let addr: SocketAddr = format!("{bind}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("HTTP API listening on http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await?;

    Ok(())
}

async fn health(_st: State<Arc<AppState>>) -> StatusCode {
    StatusCode::OK
}
