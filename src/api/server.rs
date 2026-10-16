//! axum HTTP server: bind, router, graceful shutdown.
//!
//! Stub: filled in by Task 6.

use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::sync::{mpsc::Sender as MpscSender, watch::Receiver as WatchReceiver};

use anyhow::Result;
use crate::config::Config;
use crate::common::NeoReactorCommand;

pub(crate) async fn start(
    _config_path: PathBuf,
    _config_rx: WatchReceiver<Config>,
    _command_tx: MpscSender<NeoReactorCommand>,
    _bind: String,
    _port: u16,
) -> Result<()> {
    unimplemented!("Task 6: axum server")
}
