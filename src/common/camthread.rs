use std::sync::{Arc, Weak};
use tokio::{
    sync::watch::{Receiver as WatchReceiver, Sender as WatchSender},
    time::{interval, sleep, timeout, Duration, Instant},
};
use tokio_util::sync::CancellationToken;

use crate::{config::CameraConfig, utils::connect_and_login, AnyResult};
use neolink_core::bc_protocol::BcCamera;

#[derive(Eq, PartialEq, Copy, Clone)]
pub(crate) enum NeoCamThreadState {
    Connected,
    Disconnected,
}

pub(crate) struct NeoCamThread {
    state: WatchReceiver<NeoCamThreadState>,
    config: WatchReceiver<CameraConfig>,
    cancel: CancellationToken,
    camera_watch: WatchSender<Weak<BcCamera>>,
    /// Set by run_camera after a successful login, so retry state can be reset
    logged_in: bool,
}

impl NeoCamThread {
    pub(crate) async fn new(
        watch_state_rx: WatchReceiver<NeoCamThreadState>,
        watch_config_rx: WatchReceiver<CameraConfig>,
        camera_watch_tx: WatchSender<Weak<BcCamera>>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            state: watch_state_rx,
            config: watch_config_rx,
            cancel,
            camera_watch: camera_watch_tx,
            logged_in: false,
        }
    }
    /// Connect, log in, and serve the camera until it fails, the config changes, or it is
    /// released (state goes `Disconnected`). Every exit after login logs out (SPEC P13).
    async fn run_camera(&mut self, config: &CameraConfig) -> AnyResult<Exit> {
        let name = config.name.clone();
        log::trace!("Attempting connection with config: {config:?}");
        // Until logged in, a release or config change just abandons the attempt
        let mut state = self.state.clone();
        let mut config_rec = self.config.clone();
        config_rec.borrow_and_update();
        let cancel = self.cancel.clone();
        let camera = tokio::select! {
            v = connect_and_login(config) => Arc::new(v?),
            _ = cancel.cancelled() => return Ok(Exit::Shutdown),
            Ok(_) = state.wait_for(|state| matches!(state, NeoCamThreadState::Disconnected)) => {
                return Ok(Exit::Released);
            }
            Ok(_) = config_rec.wait_for(|new| connection_changed(new, config)) => return Ok(Exit::Released),
        };
        log::trace!("  - Connected");
        self.logged_in = true;

        // Some calls answer 400 while the camera wakes up; retry those instead of sleeping a
        // fixed time before every use (SPEC P13)
        for attempt in 0..3 {
            match update_camera_time(&camera, &name, config.update_time).await {
                Err(e) if is_busy(&e) && attempt < 2 => sleep(Duration::from_secs(1)).await,
                Err(e) => {
                    log::warn!("Could not set camera time, (perhaps missing on this camera of your login in not an admin): {e:?}");
                    break;
                }
                Ok(()) => break,
            }
        }

        self.camera_watch.send_replace(Arc::downgrade(&camera));

        let cancel_check = self.cancel.clone();
        // Now we wait for a disconnect
        let exit = tokio::select! {
            _ = cancel_check.cancelled() => {
                AnyResult::Ok(Exit::Shutdown)
            }
            Ok(_) = state.wait_for(|state| matches!(state, NeoCamThreadState::Disconnected)) => {
                Ok(Exit::Released)
            }
            // Only settings that affect the connection reconnect (SPEC P14)
            Ok(_) = config_rec.wait_for(|new| connection_changed(new, config)) => {
                Ok(Exit::Released)
            }
            v = camera.join() => {
                v?;
                Ok(Exit::Shutdown)
            },
            v = async {
                let mut interval = interval(Duration::from_secs(5));
                let mut missed_pings = 0;
                loop {
                    interval.tick().await;
                    log::trace!("Sending ping");
                    match timeout(Duration::from_secs(5), camera.get_linktype()).await {
                        Ok(Ok(_)) => {
                            log::trace!("Ping reply");
                            missed_pings = 0;
                            continue
                        },
                        Ok(Err(neolink_core::Error::UnintelligibleReply { reply, why })) => {
                            // Camera does not support pings just wait forever
                            log::trace!("Pings not supported: {reply:?}: {why}");
                            futures::future::pending().await
                        },
                        Ok(Err(e)) => {
                            break Err(e.into());
                        },
                        Err(_) => {
                            // Timeout
                            if missed_pings < 5 {
                                missed_pings += 1;
                                continue;
                            } else {
                                log::error!("Timed out waiting for camera ping reply");
                                break Err(anyhow::anyhow!("Timed out waiting for camera ping reply"));
                            }
                        }
                    }
                }
            } => v,
        };
        self.camera_watch.send_replace(Weak::new());
        release(&camera).await;
        exit
    }

    // Will run and attempt to maintain the connection
    //
    // A watch sender is used to send the new camera
    // whenever it changes
    pub(crate) async fn run(&mut self) -> AnyResult<()> {
        // SPEC P4: 1, 2, 4.. 60 s while there is demand, 5 min after 10 min offline
        const MIN_BACKOFF: Duration = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(60);
        const SLOW_RETRY: Duration = Duration::from_secs(300);
        const SLOW_AFTER: Duration = Duration::from_secs(600);

        let mut backoff = MIN_BACKOFF;
        let mut failing_since: Option<Instant> = None;

        loop {
            self.state
                .clone()
                .wait_for(|state| matches!(state, NeoCamThreadState::Connected))
                .await?;
            let config = self.config.borrow().clone();
            let now = Instant::now();
            let name = config.name.clone();

            // run_camera handles release and config changes itself so that it can log out
            self.logged_in = false;
            let result = self.run_camera(&config).await;
            self.camera_watch.send_replace(Weak::new());

            if self.logged_in || now.elapsed() > Duration::from_secs(60) {
                // It worked for a while: a later failure starts the retry sequence afresh
                backoff = MIN_BACKOFF;
                failing_since = None;
            }

            match result {
                Ok(Exit::Released) => {
                    backoff = MIN_BACKOFF;
                    failing_since = None;
                    continue;
                }
                Ok(Exit::Shutdown) => {
                    // Normal shutdown
                    log::trace!("Normal camera shutdown");
                    self.cancel.cancel();
                    return Ok(());
                }
                Err(e) => {
                    // An error
                    // Check if it is non-retry
                    let e_inner = e.downcast_ref::<neolink_core::Error>();
                    match e_inner {
                        Some(neolink_core::Error::CameraLoginFail) => {
                            // Fatal
                            log::error!("{name}: Login credentials were not accepted");
                            self.cancel.cancel();
                            return Err(e);
                        }
                        _ => {
                            // Non fatal
                            let since = *failing_since.get_or_insert_with(Instant::now);
                            let wait = if since.elapsed() > SLOW_AFTER {
                                log::warn!("{name}: offline for over 10 min, retrying every 5 min");
                                SLOW_RETRY
                            } else {
                                backoff
                            };
                            log::warn!("{name}: Connection Lost: {:?}", e);
                            log::info!("{name}: Attempt reconnect in {:?}", wait);
                            // Without demand there is nothing to retry for (SPEC P4)
                            let mut state = self.state.clone();
                            tokio::select! {
                                _ = sleep(wait) => {},
                                _ = self.cancel.cancelled() => return Ok(()),
                                Ok(_) = state.wait_for(|state| matches!(state, NeoCamThreadState::Disconnected)) => {
                                    failing_since = None;
                                    backoff = MIN_BACKOFF;
                                    continue;
                                }
                            }
                            backoff = (backoff * 2).min(MAX_BACKOFF);
                        }
                    }
                }
            }
        }
    }
}

/// How a connection ended
enum Exit {
    /// Neolink is shutting down
    Shutdown,
    /// Released (idle, or config change); connect again when needed
    Released,
}

/// Whether a new config needs a new connection
fn connection_changed(new: &CameraConfig, old: &CameraConfig) -> bool {
    new.camera_addr != old.camera_addr
        || new.camera_uid != old.camera_uid
        || new.username != old.username
        || new.password != old.password
        || new.channel_id != old.channel_id
        || new.discovery != old.discovery
        || new.max_encryption != old.max_encryption
        || new.debug != old.debug
        || new.max_discovery_retries != old.max_discovery_retries
}

/// A 400 reply, which cameras send while still waking up
fn is_busy(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<neolink_core::Error>(),
        Some(neolink_core::Error::CameraServiceUnavailable { code: 400, .. })
    )
}

/// Log out and close, bounded so that a dead camera can't hold us (SPEC P13)
async fn release(camera: &BcCamera) {
    let _ = timeout(Duration::from_secs(2), camera.logout()).await;
    let _ = timeout(Duration::from_secs(2), camera.shutdown()).await;
}

impl Drop for NeoCamThread {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn update_camera_time(camera: &BcCamera, name: &str, update_time: bool) -> AnyResult<()> {
    let cam_time = camera.get_time().await?;
    let mut update = false;
    if let Some(time) = cam_time {
        log::info!("{}: Camera time is already set: {}", name, time);
        if update_time {
            update = true;
        }
    } else {
        update = true;
        log::warn!("{}: Camera has no time set, Updating", name);
    }
    if update {
        use std::time::SystemTime;
        let new_time = SystemTime::now();

        log::info!("{}: Setting time to {:?}", name, new_time);
        match camera.set_time(new_time.into()).await {
            Ok(_) => {
                let cam_time = camera.get_time().await?;
                if let Some(time) = cam_time {
                    log::info!("{}: Camera time is now set: {}", name, time);
                }
            }
            Err(e) => {
                log::error!(
                    "{}: Camera did not accept new time (is user an admin?): Error: {:?}",
                    name,
                    e
                );
            }
        }
    }
    Ok(())
}
