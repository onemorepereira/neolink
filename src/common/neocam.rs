//! This is the common code for creating a camera instance
//!
//! Features:
//!    Shared stream BC delivery
//!    Common restart code
//!    Clonable interface to share amongst threadsanyhow::anyhow;
use anyhow::Context;
use futures::{stream::StreamExt, TryFutureExt};
use std::sync::{Arc, Weak};
use tokio::{
    sync::{
        mpsc::{channel as mpsc, Sender as MpscSender},
        oneshot::{channel as oneshot, Sender as OneshotSender},
        watch::{channel as watch, Receiver as WatchReceiver, Sender as WatchSender},
    },
    task::JoinSet,
    time::{sleep, timeout, Duration},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use super::{
    LedgerStatus, LockGuard, MdRequest, MdState, NeoCamMdThread, NeoCamThread, NeoCamThreadState,
    NeoInstance, Permit, StateFile, UseCounter, WakeLedger,
};
#[cfg(feature = "pushnoti")]
use super::{PnRequest, PushNoti};
use crate::{config::CameraConfig, AnyResult, Result};
use neolink_core::bc_protocol::BcCamera;

#[allow(dead_code)]
pub(crate) enum NeoCamCommand {
    HangUp,
    Instance(OneshotSender<Result<NeoInstance>>),
    Motion(OneshotSender<WatchReceiver<MdState>>),
    Config(OneshotSender<WatchReceiver<CameraConfig>>),
    Disconnect(OneshotSender<()>),
    Connect(OneshotSender<()>),
    State(OneshotSender<NeoCamThreadState>),
    GetPermit(String, OneshotSender<Permit>),
    #[cfg(feature = "pushnoti")]
    PushNoti(OneshotSender<WatchReceiver<Option<PushNoti>>>),
    GetUid(OneshotSender<String>),
    Wakes(OneshotSender<WatchReceiver<LedgerStatus>>),
}
/// The underlying camera binding
pub(crate) struct NeoCam {
    cancel: CancellationToken,
    config_watch: WatchSender<CameraConfig>,
    commander: MpscSender<NeoCamCommand>,
    camera_watch: WatchReceiver<Weak<BcCamera>>,
    set: JoinSet<AnyResult<()>>,
    ledger: Arc<std::sync::Mutex<WakeLedger>>,
    users: WatchReceiver<u32>,
    #[allow(dead_code)]
    reasons: super::Reasons,
    state: StateFile,
    _lock: Option<LockGuard>,
}

impl NeoCam {
    pub(crate) async fn new(
        config: CameraConfig,
        #[cfg(feature = "pushnoti")] pn_request_tx: MpscSender<PnRequest>,
    ) -> Result<NeoCam> {
        let (commander_tx, commander_rx) = mpsc(100);
        let (watch_config_tx, watch_config_rx) = watch(config.clone());
        let (camera_watch_tx, camera_watch_rx) = watch(Weak::new());
        let (md_request_tx, md_request_rx) = mpsc(100);
        let (ledger_status_tx, ledger_status_rx) = watch(LedgerStatus::default());
        // A sleeping camera is not contacted at boot (SPEC P1)
        let sleeps = config.sleeps();
        if config.idle_disconnect.is_none() && config.battery.is_none() {
            log::info!(
                "{}: connects only while in use (battery or unknown power); set `battery = false` to keep a mains camera connected as before",
                config.name
            );
        }
        if config.idle_disconnect == Some(false) && config.battery != Some(false) {
            log::warn!(
                "{}: idle_disconnect is off: keeps the camera awake; expect battery drain on battery cameras",
                config.name
            );
        }
        let (state_tx, state_rx) = watch(if sleeps {
            NeoCamThreadState::Disconnected
        } else {
            NeoCamThreadState::Connected
        });
        let state = StateFile::for_camera(&config);
        let lock = state.lock();
        let (uid_tx, uid_rx) = watch(config.camera_uid.clone().or(state.load().uid));

        let set = JoinSet::new();
        let users = UseCounter::new().await;
        let users_watch = users.watch();
        let users_reasons = users.reasons();
        let test_reasons = users.reasons();

        let mut me = Self {
            cancel: CancellationToken::new(),
            config_watch: watch_config_tx,
            commander: commander_tx.clone(),
            camera_watch: camera_watch_rx.clone(),
            set,
            ledger: Default::default(),
            users: users_watch.clone(),
            reasons: test_reasons,
            state,
            _lock: lock,
        };

        // This thread recieves messages from the instances
        // and acts on it.
        //
        // This thread must be started first so that we can begin creating instances for the
        // other threads
        let sender_cancel = me.cancel.clone();
        let mut commander_rx = ReceiverStream::new(commander_rx);
        let thread_commander_tx = commander_tx.clone();
        let thread_watch_config_rx = watch_config_rx.clone();
        #[cfg(feature = "pushnoti")]
        let thread_pn_request_tx = pn_request_tx.clone();

        me.set.spawn(async move {
            let thread_cancel = sender_cancel.clone();
            let res = tokio::select! {
                _ = sender_cancel.cancelled() => {
                    Result::Ok(())
                },
                v = async {
                    while let Some(command) = commander_rx.next().await {
                        match command {
                            NeoCamCommand::HangUp => {
                                sender_cancel.cancel();
                                return Result::<(), anyhow::Error>::Ok(());
                            }
                            NeoCamCommand::Instance(result) => {
                                let instance = NeoInstance::new(
                                    camera_watch_rx.clone(),
                                    thread_commander_tx.clone(),
                                    thread_cancel.clone(),
                                );
                                let _ = result.send(instance);
                            }
                            NeoCamCommand::Motion(sender) => {
                                md_request_tx.send(
                                    MdRequest::Get {
                                        sender,
                                    }
                                ).await?;
                            },
                            NeoCamCommand::Config(sender) => {
                                let _ = sender.send(thread_watch_config_rx.clone());
                            },
                            NeoCamCommand::Connect(sender) => {
                                if !matches!(*state_tx.borrow(), NeoCamThreadState::Connected) {
                                    state_tx.send_replace(NeoCamThreadState::Connected);
                                }
                                let _ = sender.send(());
                            }
                            NeoCamCommand::Disconnect(sender) => {
                                if !matches!(*state_tx.borrow(), NeoCamThreadState::Disconnected) {
                                    state_tx.send_replace(NeoCamThreadState::Disconnected);
                                }
                                let _ = sender.send(());
                            }
                            NeoCamCommand::State(sender) => {
                                let _ = sender.send(*state_tx.borrow());
                            }
                            NeoCamCommand::GetPermit(reason, sender) => {
                                let _ = sender.send(users.create_activated_for(&reason).await?);
                            }
                            #[cfg(feature = "pushnoti")]
                            NeoCamCommand::PushNoti(sender) => {
                                thread_pn_request_tx.send(
                                    PnRequest::Get {
                                        sender,
                                    }
                                ).await?;
                            },
                            NeoCamCommand::Wakes(sender) => {
                                let _ = sender.send(ledger_status_rx.clone());
                            }
                            NeoCamCommand::GetUid(sender) => {
                                let mut uid_rx = uid_rx.clone();
                                tokio::task::spawn(async move {
                                    let uid = uid_rx.wait_for(|v| v.is_some()).await?.clone().unwrap();
                                    let _ = sender.send(uid);
                                    AnyResult::Ok(())
                                });
                            },
                        }
                    }
                    Ok(())
                } => {
                    v
                }
            };
            res
        });

        // This gets the first instance which we use for making the other threads
        let (instance_tx, instance_rx) = oneshot();
        commander_tx
            .send(NeoCamCommand::Instance(instance_tx))
            .await?;
        let instance = instance_rx.await??;

        // This thread maintains the camera loop
        //
        // It will keep it logged and reconnect
        let thread_watch_config_rx = watch_config_rx.clone();
        let mut cam_thread = NeoCamThread::new(
            state_rx,
            thread_watch_config_rx,
            camera_watch_tx,
            me.cancel.clone(),
        )
        .await;
        me.set.spawn(async move { cam_thread.run().await });

        // This thread monitors the motion
        let md_instance = instance.subscribe().await?;
        let md_cancel = me.cancel.clone();
        let mut md_thread = NeoCamMdThread::new(md_request_rx, md_instance).await?;
        me.set.spawn(async move {
            tokio::select! {
                _ = md_cancel.cancelled() => AnyResult::Ok(()),
                v = md_thread.run() => {
                    v
                },
            }
        });

        // On every wake, piggyback the reads that would otherwise need their own wake:
        // battery, UID, model/firmware, streams, and a snapshot (SPEC P5, P7, P15). This is
        // passive: it never causes or extends a wake.
        let wake_instance = instance.subscribe().await?;
        let wake_cancel = me.cancel.clone();
        let wake_name = config.name.clone();
        let wake_state = me.state.clone();
        me.set.spawn(async move {
            tokio::select! {
                _ = wake_cancel.cancelled() => AnyResult::Ok(()),
                v = async {
                    let mut done = WakeReads::default();
                    loop {
                        wake_instance.next_connection().await?;
                        wake_reads(&wake_instance, &wake_state, &uid_tx, &wake_name, &mut done).await;
                    }
                } => v,
            }
        });

        // Handles push notifications
        #[cfg(feature = "pushnoti")]
        {
            let pn_root_instance = instance.subscribe().await?;
            let pn_cancel = me.cancel.clone();
            let thread_pn_request_tx = pn_request_tx.clone();
            me.set.spawn(async move {
                tokio::select!{
                    _ = pn_cancel.cancelled() => {
                        AnyResult::Ok(())
                    },
                    v = async {
                        let mut config_rx = pn_root_instance.config().await?;
                        loop {
                            // Wait for the green light
                            config_rx.wait_for(|config| config.push_notifications).await?;

                            // Activate it
                            let pn_instance = pn_root_instance.subscribe().await?;
                            let (tx, rx) = oneshot();
                            thread_pn_request_tx.send(PnRequest::Activate{sender: tx, instance: pn_instance}).await?;
                            rx.await??;

                            let pn_permit_instance = pn_root_instance.subscribe().await?;
                            let r = tokio::select! {
                                // Push notification permits
                                v = async {
                                    let mut prev_noti = None;
                                    let mut pn = pn_permit_instance.push_notifications().await?;
                                    loop{
                                        prev_noti = pn.wait_for(|noti| noti != &prev_noti && noti.is_some()).await.map(|noti| noti.clone())?;
                                        // A push only keeps the camera connected when something
                                        // consumes motion (SPEC P12)
                                        let wanted = {
                                            let c = pn_permit_instance.config().await?;
                                            let c = c.borrow();
                                            c.pause.on_motion || !c.sleeps()
                                        };
                                        if wanted {
                                            let _permit = pn_permit_instance.permit("pushnoti").await?;
                                            sleep(Duration::from_secs(30)).await; // Push notification will wake us up for 30s
                                        }
                                    }
                                } => v,
                                // Continue loop on Red light
                                v = config_rx.wait_for(|config| !config.push_notifications).map_ok(|_| ()) => {
                                    v?;
                                    AnyResult::Ok(())
                                },
                            };
                            if r.is_err() {
                                break r;
                            }
                        }?;
                        AnyResult::Ok(())
                    } => {
                        v
                    },
                }
            });
        }

        // MD permits: motion keeps the camera connected only when something consumes motion
        // (pause.on_motion), for at most motion_hold_max_s per event (SPEC P12)
        let md_permit_instance = instance.subscribe().await?;
        let md_permit_cancel = me.cancel.clone();
        me.set.spawn(async move {
            tokio::select! {
                _ = md_permit_cancel.cancelled() => {
                    AnyResult::Ok(())
                },
                v = async {
                    let mut config_rx = md_permit_instance.config().await?;
                    let mut md = md_permit_instance.motion().await.with_context(|| "Unable to acquire motion watcher")?;
                    loop {
                        config_rx.wait_for(|config| config.pause.on_motion).await.map(|_| ())?;
                        let hold_max = Duration::from_secs(config_rx.borrow().motion_hold_max_s);
                        tokio::select! {
                            v = config_rx.wait_for(|config| !config.pause.on_motion).map_ok(|_| ()) => { v?; continue; }
                            v = md.wait_for(|md| matches!(md, MdState::Start(_))).map_ok(|_| ()) => { v.with_context(|| "MD Watcher lost")?; }
                        }
                        // A Start that reaches us after its connection dropped is stale: the
                        // reset to Unknown already ran, and acting on it would reconnect
                        if md_permit_instance.camera().borrow().upgrade().is_none() {
                            md.wait_for(|md| !matches!(md, MdState::Start(_))).await.map(|_| ()).with_context(|| "MD Watcher lost")?;
                            continue;
                        }
                        let _permit = md_permit_instance.permit("motion").await.with_context(|| "Unable to acquire motion permit")?;
                        tokio::select! {
                            _ = sleep(hold_max) => {
                                log::info!("Motion held the camera for {hold_max:?}, releasing");
                                // Wait for this event to end before taking a new permit
                                md.wait_for(|md| !matches!(md, MdState::Start(_))).await.map(|_| ()).with_context(|| "MD Watcher lost")?;
                                continue;
                            }
                            // Stop, or Unknown after a disconnect, releases it
                            v = md.wait_for(|md| !matches!(md, MdState::Start(_))).map_ok(|_| ()) => { v.with_context(|| "MD Watcher lost")?; }
                        }
                        // After a disconnect there is no linger: holding on would reconnect
                        if matches!(*md.borrow(), MdState::Unknown) {
                            continue;
                        }
                        // Try waiting for 30s
                        // If in those 30s we get motion then return to
                        // loop early to reaquire the permit
                        tokio::select!{
                            _ = sleep(Duration::from_secs(30)) => {},
                            v = md.wait_for(|md| matches!(md, MdState::Start(_))).map_ok(|_| ()) => {v.with_context(|| "MD Watcher lost")?;},
                        }
                    }
                } => {
                    v
                },
            }
        });

        // This thread connects the camera while permits are held and disconnects it
        // idle_grace_s after the last one is dropped, from boot on (SPEC P1, P2). It observes
        // the use counter without holding a permit (SPEC P10) and keeps the wake ledger
        // (SPEC P6).
        let connect_instance = instance.subscribe().await?;
        let connect_cancel = me.cancel.clone();
        let ledger_name = config.name.clone();
        let ledger = me.ledger.clone();
        let ledger_tx = ledger_status_tx;
        me.set.spawn(async move {
            tokio::select!{
                _ = connect_cancel.cancelled() => {
                    AnyResult::Ok(())
                },
                v = async {
                    let mut config_rx = connect_instance.config().await?;
                    let mut users = users_watch;
                    loop {
                        if !config_rx.borrow_and_update().sleeps() {
                            connect_instance.connect().await?;
                            let _ = ledger.lock().unwrap().wake("always-on".to_string());
                            ledger_tx.send_replace(ledger.lock().unwrap().status());
                            config_rx.wait_for(|config| config.sleeps()).await.map(|_| ())?;
                            // Switched to sleeping while connected: fall through to the
                            // release below (grace, then disconnect) instead of waiting
                            // for a use that may never come
                        } else {
                            tokio::select! {
                                v = config_rx.wait_for(|config| !config.sleeps()).map_ok(|_| ()) => { v?; continue; }
                                v = users.wait_for(|n| *n > 0).map_ok(|_| ()) => { v?; }
                            }
                            let reason = {
                                let reasons = users_reasons.lock().unwrap();
                                let mut r: Vec<&str> = reasons.values().map(|s| s.as_str()).collect();
                                r.sort_unstable();
                                r.dedup();
                                r.join(",")
                            };
                            let warning = ledger.lock().unwrap().wake(reason);
                            if let Some(warning) = warning {
                                log::warn!("{ledger_name}: {warning}");
                            }
                            ledger_tx.send_replace(ledger.lock().unwrap().status());
                            connect_instance.connect().await?;
                        }
                        loop {
                            users.wait_for(|n| *n == 0).await.map(|_| ())?;
                            let grace = Duration::from_secs(config_rx.borrow().idle_grace_s);
                            tokio::select! {
                                _ = sleep(grace) => break,
                                v = users.wait_for(|n| *n > 0).map_ok(|_| ()) => { v?; continue; }
                            };
                        }
                        connect_instance.disconnect().await?;
                        let summary = ledger.lock().unwrap().sleep();
                        if let Some(summary) = summary {
                            log::info!("{ledger_name}: {summary}");
                        }
                        ledger_tx.send_replace(ledger.lock().unwrap().status());
                    }
                } => {
                    v
                },
            }
        });

        Ok(me)
    }

    pub(crate) async fn subscribe(&self) -> Result<NeoInstance> {
        NeoInstance::new(
            self.camera_watch.clone(),
            self.commander.clone(),
            self.cancel.clone(),
        )
    }

    /// Active permits right now (PLAN 1.10)
    #[allow(dead_code)]
    pub(crate) fn use_count(&self) -> u32 {
        *self.users.borrow()
    }

    /// Why permits are held right now
    #[allow(dead_code)]
    pub(crate) fn permit_reasons(&self) -> Vec<String> {
        self.reasons.lock().unwrap().values().cloned().collect()
    }

    /// The camera's state file (SPEC P15)
    #[allow(dead_code)]
    pub(crate) fn state(&self) -> StateFile {
        self.state.clone()
    }

    /// The wake ledger (SPEC P6)
    #[allow(dead_code)]
    pub(crate) fn ledger(&self) -> Arc<std::sync::Mutex<WakeLedger>> {
        self.ledger.clone()
    }

    /// Stop every task, logging the camera out if it is connected or logging in (PLAN 1.5);
    /// ≤ 5 s (the camera thread's own bound is 4.5 s)
    pub(crate) async fn shutdown(&mut self) {
        self.cancel.cancel();
        let set = &mut self.set;
        let joined = tokio::time::timeout(Duration::from_secs(5), async {
            while set.join_next().await.is_some() {}
        })
        .await;
        if joined.is_err() {
            log::warn!("Camera tasks did not stop within 5 s");
            self.set.abort_all();
        }
    }

    /// Only a changed config is pushed, so re-publishing the same config does not
    /// reconnect anything (SPEC P14)
    pub(crate) async fn update_config(&self, config: CameraConfig) -> Result<()> {
        self.config_watch.send_if_modified(|old| {
            if *old != config {
                *old = config;
                true
            } else {
                false
            }
        });
        Ok(())
    }
}

impl Drop for NeoCam {
    fn drop(&mut self) {
        log::trace!("Drop NeoCam");
        let mut set = std::mem::take(&mut self.set);
        let commander = self.commander.clone();
        let _gt = tokio::runtime::Handle::current().enter();
        tokio::task::spawn(async move {
            let _ = commander.send(NeoCamCommand::HangUp).await;
            while set.join_next().await.is_some() {}
            log::trace!("Dropped NeoCam");
        });
    }
}

/// Per-call deadline for each step in `wake_reads`. Long enough for a slow camera on a
/// relay connection; short enough that one hung command can't block the rest of the reads
/// for the entire wake.
const WAKE_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// What the wake reads have learnt, and which failures were already reported
#[derive(Default)]
struct WakeReads {
    version: bool,
    streams: bool,
    /// Once a snapshot has been saved, or the camera has permanently refused SNAP
    /// (`CameraServiceUnavailable`), stop trying every wake.
    snapshot: bool,
    warned: std::collections::HashSet<&'static str>,
}

impl WakeReads {
    /// A failed read: warn the first time, then only at debug (it is tried every wake)
    fn failed(&mut self, name: &str, what: &'static str, e: &anyhow::Error) {
        if self.warned.insert(what) {
            log::warn!(
                "{name}: could not read the {what} while awake (tried again on later wakes): {e:#}"
            );
        } else {
            log::debug!("{name}: could not read the {what}: {e:#}");
        }
    }
}

/// The reads done once per wake (see the wake thread in `NeoCam::new`). Each read stands
/// alone: one the camera won't answer must not cost the others, or the snapshot endpoint
/// and MQTT would have nothing to serve for this camera, which tempts users into waking it
/// (SPEC P5, P7)
async fn wake_reads(
    instance: &NeoInstance,
    state: &StateFile,
    uid_tx: &WatchSender<Option<String>>,
    name: &str,
    done: &mut WakeReads,
) {
    // The Argus rejects battery reads (400) for a while after waking (SPEC §9), longer
    // than run_passive_task's own retries: try first, then again after the other reads
    let mut battery_done = read_battery(instance, state, name).await;

    if uid_tx.borrow().is_none() {
        match timeout(
            WAKE_READ_TIMEOUT,
            instance.run_passive_task(|cam| Box::pin(async move { Ok(cam.uid().await?) })),
        )
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out after {WAKE_READ_TIMEOUT:?}")))
        {
            Ok(uid) => {
                if let Err(e) = state.update(|s| s.uid = Some(uid.clone())) {
                    log::warn!("{name}: could not save the camera state: {e:?}");
                }
                uid_tx.send_replace(Some(uid));
            }
            Err(e) => done.failed(name, "UID", &e),
        }
    }

    if !done.version {
        match timeout(
            WAKE_READ_TIMEOUT,
            instance.run_passive_task(|cam| Box::pin(async move { Ok(cam.version().await?) })),
        )
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out after {WAKE_READ_TIMEOUT:?}")))
        {
            Ok(version) => {
                log::info!(
                    "{name}: Model {}",
                    version.model.clone().unwrap_or("Undeclared".to_string())
                );
                log::info!("{name}: Firmware Version {}", version.firmwareVersion);
                if let Err(e) = state.update(|s| {
                    s.model = version.model.clone();
                    s.firmware = Some(version.firmwareVersion.clone());
                }) {
                    log::warn!("{name}: could not save the camera state: {e:?}");
                }
                done.version = true;
            }
            Err(e) => done.failed(name, "model and firmware", &e),
        }
    }

    if !done.streams {
        match timeout(
            WAKE_READ_TIMEOUT,
            instance
                .run_passive_task(|cam| Box::pin(async move { Ok(cam.get_stream_info().await?) })),
        )
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out after {WAKE_READ_TIMEOUT:?}")))
        {
            Ok(stream_info) => {
                let streams: Vec<String> = stream_info
                    .stream_infos
                    .iter()
                    .flat_map(|i| i.encode_tables.iter().map(|e| e.name.clone()))
                    .collect();
                if let Err(e) = state.update(|s| s.supported_streams = streams.clone()) {
                    log::warn!("{name}: could not save the camera state: {e:?}");
                }
                done.streams = true;
            }
            Err(e) => done.failed(name, "stream list", &e),
        }
    }

    // One snapshot per wake, for the snapshot endpoint and the CLI (SPEC P5).
    // Skipped once saved successfully or once the camera has permanently refused or
    // ignored SNAP (any error or timeout), to avoid retrying on every wake.
    if !done.snapshot {
        match timeout(
            WAKE_READ_TIMEOUT,
            instance
                .run_passive_task(|cam| Box::pin(async move { Ok(cam.get_snapshot().await?) })),
        )
        .await
        {
            Ok(Ok(jpeg)) => {
                if let Err(e) = state.save_snapshot(&jpeg) {
                    log::warn!("{name}: could not save the snapshot: {e:?}");
                }
                done.snapshot = true;
            }
            // Any failure (timeout, error, unsupported) — stop trying every wake
            Ok(Err(e)) => {
                done.snapshot = true;
                done.failed(name, "snapshot", &e);
            }
            Err(_) => {
                done.snapshot = true;
                done.failed(
                    name,
                    "snapshot",
                    &anyhow::anyhow!("timed out after {WAKE_READ_TIMEOUT:?}"),
                );
            }
        }
    }

    for _ in 0..2 {
        if battery_done {
            break;
        }
        sleep(Duration::from_secs(5)).await;
        battery_done = read_battery(instance, state, name).await;
    }
    if !battery_done {
        log::info!("{name}: no battery reading this wake");
    }
}

/// One battery read into the state file; false if the camera didn't answer.
/// Tries the polled `battery_info` command first; if it is rejected or times out,
/// falls back to `battery_info_list` — the unsolicited push some cameras (e.g. Argus
/// PT Ultra) send right after login instead of responding to the polled request.
async fn read_battery(instance: &NeoInstance, state: &StateFile, name: &str) -> bool {
    use super::state::BatteryReading;
    // Bounded, so a wake that ends meanwhile doesn't leave this waiting for the next one
    let polled = tokio::time::timeout(
        Duration::from_secs(15),
        instance.run_passive_task(|cam| Box::pin(async move { Ok(cam.battery_info().await?) })),
    )
    .await;
    let result = match polled {
        Ok(Ok(b)) => Ok(b),
        Ok(Err(e)) => {
            log::debug!("{name}: polled battery read failed ({e:#}); trying login-push fallback");
            tokio::time::timeout(
                Duration::from_secs(15),
                instance.run_passive_task(|cam| {
                    Box::pin(async move { Ok(cam.battery_info_list().await?) })
                }),
            )
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")))
        }
        Err(_) => {
            log::debug!("{name}: polled battery timed out; trying login-push fallback");
            tokio::time::timeout(
                Duration::from_secs(15),
                instance.run_passive_task(|cam| {
                    Box::pin(async move { Ok(cam.battery_info_list().await?) })
                }),
            )
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")))
        }
    };
    let b = match result {
        Ok(b) => b,
        Err(e) => {
            log::debug!("{name}: battery read failed: {e:?}");
            return false;
        }
    };
    log::info!(
        "{name}: battery {}% ({} mV, {} mA, {})",
        b.battery_percent,
        b.voltage,
        b.current,
        b.charge_status
    );
    let saved = state.update(|s| {
        s.battery = Some(true);
        s.last_battery = Some(BatteryReading::now(&b));
    });
    if let Err(e) = saved {
        log::warn!("{name}: could not save the battery reading: {e:?}");
    }
    true
}
