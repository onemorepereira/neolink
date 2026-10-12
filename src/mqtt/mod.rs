//!
//! # Neolink MQTT
//!
//! Handles incoming and outgoing MQTT messages
//!
//! This acts as a bridge between cameras and MQTT servers
//!
//! Messages are prefixed with `neolink/{CAMERANAME}`
//!
//! Control messages:
//!
//! - `/control/floodlight [on|off]` Turns floodlight (if equipped) on/off
//! - `/control/led [on|off]` Turns status LED on/off
//! - `/control/pir [on|off]` Turns PIR on/off
//! - `/control/ir [on|off|auto]` Turn IR lights on/off or automatically via light detection
//! - `/control/reboot` Reboot the camera
//! - `/control/ptz` [up|down|left|right|in|out] (amount) Control the PTZ movements, amount defaults to 32.0
//! - `/control/ptz/preset` [id] Move the camera to a known preset
//! - `/control/ptz/assign` [id] [name] Assign the current ptz position to an ID and name
//!
//! Status Messages:
//!
//! `/status offline` Sent when the neolink goes offline this is a LastWill message
//! `/status disconnected` Sent when the camera goes offline
//! `/status/battery` Sent in reply to a `/query/battery`
//! `/status/pir` Sent in reply to a `/query/pir`
//! `/status/ptz/preset` Sent in reply to a `/query/ptz/preset`
//!
//! Query Messages:
//!
//! `/query/battery` Request that the camera reports its battery level
//! `/query/pir` Request that the camera reports its pir status
//! `/query/ptz/preset` Request that the camera reports the PTZ presets
//! `/query/preview` Request that the camera post a base64 encoded jpeg
//!    of the stream to `/status/preview`
//!
//! On a sleeping camera queries answer from its last wake and wake it only for the payload
//! `fresh`, and `/control/wakeup` is bounded (see `power.rs`).
//!
//!
//! # Usage
//!
//! ```bash
//! neolink mqtt --config=config.toml
//! ```
//!
//! # Example Config
//!
//! ```toml
//! [[cameras]]
//! name = "Cammy"
//! username = "****"
//! password = "****"
//! address = "****:9000"
//!   [cameras.mqtt]
//!   server = "127.0.0.1"
//!   port = 1883
//!   credentials = ["username", "password"]
//! ```
//!
//! `server` is the mqtt server
//! `port` is the mqtt server's port
//! `credentials` are the username and password required to identify with the mqtt server
//!
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use std::collections::{HashMap, HashSet};
use tokio::{
    sync::mpsc::channel as mpsc,
    task::JoinSet,
    time::{interval, sleep, Duration, MissedTickBehavior},
};
use tokio_stream::{wrappers::IntervalStream, StreamExt};
use tokio_util::sync::CancellationToken;

use neolink_core::bc_protocol::{Direction as BcDirection, LightState};

mod cmdline;
mod configwrite;
mod discovery;
mod mqttc;
mod power;
#[cfg(test)]
mod tests;

use crate::{
    common::{unix_now, BatteryReading, MdState, NeoInstance, NeoReactor, StateFile},
    AnyResult,
};
use anyhow::{anyhow, Context, Result};
pub(crate) use cmdline::Opt;
pub(crate) use discovery::Discoveries;
use log::*;
use mqttc::{Mqtt, MqttReplyRef};

use self::{
    configwrite::{prepare_config_write, ConfigWrite},
    discovery::enable_discovery,
    mqttc::{MqttInstance, MqttReply},
    power::{wants_fresh, CamPower, FRESH_MAX_AGE_S},
};

/// Entry point for the mqtt subcommand
///
/// Opt is the command line options
pub(crate) async fn main(_: Opt, reactor: NeoReactor) -> Result<()> {
    let mut set = tokio::task::JoinSet::new();
    let global_cancel = CancellationToken::new();
    let cancel_drop = global_cancel.clone().drop_guard();
    let config = reactor.config().await?;
    {
        let c = config.borrow();
        if c.mqtt_config_writes && !c.users.is_empty() {
            log::warn!(
                "MQTT clients can rewrite the config, including users and permitted_users, \
                 on the neolink/config topic; set mqtt_config_writes = false to prevent that"
            );
        }
    }
    let mqtt = Mqtt::new(config.clone()).await;

    // Startup and stop cameras as they are added/removed to the config
    let thread_cancel = global_cancel.clone();
    let mut thread_config = config.clone();
    let thread_reactor = reactor.clone();
    let thread_instance = mqtt.subscribe("").await?;
    set.spawn(async move {
        let mut set = JoinSet::<AnyResult<()>>::new();
        let thread_cancel2 = thread_cancel.clone();
        tokio::select!{
            _ = thread_cancel.cancelled() => AnyResult::Ok(()),
            v = async {
                let mut cameras: HashMap<String, CancellationToken> = Default::default();
                let mut config_names = HashSet::new();
                loop {
                    thread_config.wait_for(|config| {
                        let current_names = config.cameras.iter().filter(|a| a.enabled).map(|cam_config| cam_config.name.clone()).collect::<HashSet<_>>();
                        current_names != config_names
                    }).await.with_context(|| "Camera Config Watcher")?;
                    config_names = thread_config.borrow().clone().cameras.iter().filter(|a| a.enabled).map(|cam_config| cam_config.name.clone()).collect::<HashSet<_>>();

                    for name in config_names.iter() {
                        log::info!("{name}: MQTT Starting");
                        if ! cameras.contains_key(name) {
                            let local_cancel = CancellationToken::new();
                            cameras.insert(name.clone(),local_cancel.clone());

                            let thread_global_cancel = thread_cancel2.clone();
                            let thread_reactor2 = thread_reactor.clone();
                            let mqtt_instance = thread_instance.subscribe(name).await?;
                            let name = name.clone();
                            set.spawn(async move {
                                loop {
                                    let camera = thread_reactor2.get(&name).await?;
                                    let mqtt_instance = mqtt_instance.resubscribe().await?;
                                    let r = tokio::select!{
                                        _ = thread_global_cancel.cancelled() => {
                                            AnyResult::Ok(())
                                        },
                                        _ = local_cancel.cancelled() => {
                                            AnyResult::Ok(())
                                        },
                                        v = listen_on_camera(camera, mqtt_instance) => {
                                            v
                                        },
                                    };
                                    if let Ok(()) = &r {
                                        break r
                                    } else {
                                        continue;
                                    }
                                }
                            }) ;
                        }
                    }

                    for (running_name, token) in cameras.iter() {
                        if ! config_names.contains(running_name) {
                            token.cancel();
                        }
                    }
                }
            } => v,
        }
    });

    // This threads prints the config
    let mut thread_config = config.clone();
    let thread_instance = mqtt.subscribe("").await?;
    let thread_cancel = global_cancel.clone();
    set.spawn(async move {
        tokio::select! {
            _ = thread_cancel.cancelled() => AnyResult::Ok(()),
            v = async {
                let mut curr_config = thread_config.borrow().clone();
                let str = toml::to_string(&curr_config)?;
                thread_instance.send_message("config", &str, true).await?;
                loop {
                    curr_config = thread_config
                        .wait_for(|new_conf| new_conf != &curr_config)
                        .await?
                        .clone();
                    let str = toml::to_string(&curr_config)?;
                    thread_instance.send_message("config", &str, true).await?;
                    log::trace!("UpdatedPosted config");
                }
            } => v,
        }
    });

    // This threads checks for config changes on the mqtt
    let thread_config = config.clone();
    // Only the config file sets power settings: MQTT is the only other writer, and this
    // copy is taken before it can write (SPEC P14)
    let file_config = config.borrow().clone();
    let mut thread_instance = mqtt.subscribe("").await?;
    let thread_reactor = reactor.clone();
    let thread_cancel = global_cancel.clone();
    set.spawn(async move {
        tokio::select! {
            _ = thread_cancel.cancelled() => AnyResult::Ok(()),
            v = async {
                while let Ok(msg) = thread_instance.recv().await {
                    if msg.topic == "config" {
                        // SPEC R29: the topic can rewrite users and permissions
                        if !thread_config.borrow().mqtt_config_writes {
                            log::warn!("Ignored a config write over MQTT: mqtt_config_writes = false");
                            thread_instance
                                .send_message(
                                    "config/status",
                                    "Rejected: config writes over MQTT are disabled (mqtt_config_writes = false)",
                                    false,
                                )
                                .await?;
                            continue;
                        }
                        let current = thread_config.borrow().clone();
                        let config = match prepare_config_write(&msg.message, &file_config, &current) {
                            ConfigWrite::Unchanged => continue,
                            ConfigWrite::Refused(why) => {
                                thread_instance.send_message("config/status", &why, false).await?;
                                continue;
                            }
                            ConfigWrite::Apply(config) => *config,
                        };
                        let result = thread_reactor.update_config(config).await;
                        thread_instance
                            .send_message("config/status", &format!("{:?}", result), false)
                            .await?;
                        log::info!("Updated config");
                    }
                }
                AnyResult::Ok(())
            } => v,
        }
    });

    while let Some(result) = set.join_next().await {
        if let Err(_) | Ok(Err(_)) = &result {
            global_cancel.cancel();
            result??;
        }
    }

    drop(cancel_drop);
    Ok(())
}

async fn listen_on_camera(camera: NeoInstance, mqtt_instance: MqttInstance) -> Result<()> {
    let mut watch_config = camera.config().await?;
    let camera_name = watch_config.borrow().name.clone();
    let mut config;
    let cancel = CancellationToken::new();
    let drop_cancel = cancel.clone().drop_guard();
    let r = loop {
        config = watch_config.borrow().clone().mqtt;
        break tokio::select! {
            v = watch_config.wait_for(|new_config| config != new_config.mqtt) => {
                v?;
                continue;
            }
            v = async {
                //Publish initial states
                mqtt_instance
                        .send_message("status", "disconnected", true)
                        .await
                        .with_context(|| format!("Failed to publish status for {}", camera_name))?;
                let _drop_message = mqtt_instance.last_will("status", "disconnected").await?;
                mqtt_instance
                    .send_message("status/motion", "unknown", true)
                    .await
                    .with_context(|| format!("Failed to publish motion unknown for {}", camera_name))?;
                mqtt_instance
                    .send_message("status/notification", "unknown", true)
                    .await
                    .with_context(|| format!("Failed to publish push notification unknown for {}", camera_name))?;
                let _drop_message2 = mqtt_instance.last_will("status/motion", "unknown").await?;

                if let Some(discovery_config) = config.discovery.as_ref() {
                    enable_discovery(discovery_config, &mqtt_instance, &camera).await?;
                }

                let camera_msg = camera.clone();
                let mut mqtt_msg = mqtt_instance.resubscribe().await?;
                let cancel_msg = cancel.clone();
                let mut set_msg = JoinSet::new();

                let mut camera_watch = camera.camera();
                let mqtt_watch = mqtt_instance.resubscribe().await?;

                let camera_floodlight = camera.clone();
                let mqtt_floodlight = mqtt_instance.resubscribe().await?;

                let camera_motion = camera.clone();
                let mqtt_motion = mqtt_instance.resubscribe().await?;

                #[cfg(feature = "pushnoti")]
                let camera_pn = camera.clone();
                #[cfg(feature = "pushnoti")]
                let mqtt_pn = mqtt_instance.resubscribe().await?;

                let camera_snap = camera.clone();
                let mqtt_snap = mqtt_instance.resubscribe().await?;

                let camera_battery = camera.clone();
                let mqtt_battery = mqtt_instance.resubscribe().await?;

                let camera_floodlight_tasks = camera.clone();
                let mqtt_floodlight_tasks = mqtt_instance.resubscribe().await?;

                // control/wakeup and coalesced queries: ends with this listener
                let power = std::sync::Arc::new(CamPower::new());
                let camera_wakeup = camera.clone();

                let mut wakes = camera.wakes().await?;
                let mqtt_wakes = mqtt_instance.resubscribe().await?;

                tokio::select! {
                    _ = cancel.cancelled() => AnyResult::Ok(()),
                    // Handles incomming requests
                    v  = async {
                        let (tx, mut rx) = mpsc(1);
                        tokio::select! {
                            v = async {
                                log::debug!("Listening to message on {}", mqtt_msg.get_name());
                                while let Ok(msg) = mqtt_msg.recv().await {
                                    let mqtt_msg = mqtt_msg.resubscribe().await?;
                                    let camera_msg = camera_msg.clone();
                                    let tx = tx.clone();
                                    let cancel_msg = cancel_msg.clone();
                                    let power = power.clone();
                                    set_msg.spawn(async move {
                                        tokio::select!{
                                            _ = cancel_msg.cancelled() => AnyResult::Ok(()),
                                            v = async {
                                                let res = handle_mqtt_message(msg, &mqtt_msg, &camera_msg, &power).await;
                                                if res.is_err() {
                                                    tx.send(res).await?;
                                                }
                                                AnyResult::Ok(())
                                            } => v,
                                        }
                                    });
                                }
                                AnyResult::Ok(())
                            } => {
                                v
                            },
                            v = rx.recv() => {
                                v.ok_or(anyhow!("All error senders were dropped"))?
                            },
                        }?;
                        AnyResult::Ok(())
                    } => v,
                    v = power.run_wakeups(&camera_wakeup, &camera_name) => v,
                    // The wake ledger (SPEC P6), retained so it survives HA restarts
                    v = async {
                        loop {
                            let status = wakes.borrow_and_update().clone();
                            if status.since.is_some() {
                                mqtt_wakes
                                    .send_message("status/wakes", &serde_json::to_string(&status)?, true)
                                    .await
                                    .with_context(|| format!("{}: Failed to publish the wake ledger", camera_name))?;
                            }
                            wakes.changed().await.with_context(|| format!("{}: Ledger Watch Dropped", camera_name))?;
                        }
                    } => v,
                    // Handle camera disconnect/connect
                    v = async {
                        loop {
                            camera_watch.wait_for(|cam| cam.upgrade().is_some()).await.with_context(|| {
                                format!("{}: Online Watch Dropped", camera_name)
                            })?;
                            log::trace!("Publish online");
                            mqtt_watch.send_message("status", "connected", true).await.with_context(|| {
                                format!("{}: Failed to publish connected", camera_name)
                            })?;
                            camera_watch.wait_for(|cam| cam.upgrade().is_none()).await.with_context(|| {
                                format!("{}: Disconnect Watch Dropped", camera_name)
                            })?;
                            mqtt_watch.send_message("status", "disconnected", true).await.with_context(|| {
                                format!("{}: Failed to publish disconnected", camera_name)
                            })?;
                        }
                    } => {
                        v
                    },
                    // Handle the floodlight
                    v = async {
                        let (tx, mut rx) = mpsc(100);
                        let v = tokio::select! {
                            v = async {
                                loop {
                                    let r = camera_floodlight.run_passive_task(|cam| {
                                        let tx = tx.clone();
                                        Box::pin(
                                            async move {
                                                let mut reciever = tokio_stream::wrappers::ReceiverStream::new(cam.listen_on_flightlight().await?);
                                                while let Some(flights) = reciever.next().await {
                                                    for flight in flights.floodlight_status_list.iter() {
                                                        if flight.status == 0 {
                                                            tx.send(false).await?;
                                                        } else {
                                                            tx.send(true).await?;
                                                        }
                                                    }
                                                }
                                                AnyResult::Ok(())
                                            }
                                        )
                                    }).await;
                                    if r.is_err() {
                                        break r;
                                    }
                                }
                            } => v,
                            v = async {
                                while let Some(on) = rx.recv().await {
                                    if on {
                                        mqtt_floodlight.send_message("status/floodlight", "on", true).await?;
                                    } else {
                                        mqtt_floodlight.send_message("status/floodlight", "off", true).await?;
                                    }
                                }
                                AnyResult::Ok(())
                            } => v,
                        };
                        match v.map_err(|e| e.downcast::<neolink_core::Error>()) {
                            Err(Ok(neolink_core::Error::UnintelligibleReply{..})) => futures::future::pending().await,
                            Ok(()) => AnyResult::Ok(()),
                            Err(Ok(e)) => Err(e.into()),
                            Err(Err(e)) => Err(e),
                        }?;
                        AnyResult::Ok(())
                    }, if config.enable_light => v,
                    // Handle the motion messages
                    v = async {
                        let mut md = camera_motion.motion().await?;
                        loop {
                            let v = async {
                                md.wait_for(|state| matches!(state, MdState::Start(_))).await.with_context(|| {
                                    format!("{}: MdStart Watch Dropped", camera_name)
                                })?;
                                mqtt_motion.send_message("status/motion", "on", true).await.with_context(|| {
                                    format!("{}: Failed to publish motion start", camera_name)
                                })?;
                                // Stop, or Unknown when the camera went to sleep or dropped
                                // mid-motion: nothing reports the end then, and a stuck `on`
                                // makes automations wake the camera again
                                md.wait_for(|state| !matches!(state, MdState::Start(_))).await.with_context(|| {
                                    format!("{}: MdStop Watch Dropped", camera_name)
                                })?;
                                mqtt_motion.send_message("status/motion", "off", true).await.with_context(|| {
                                    format!("{}: Failed to publish motion stop", camera_name)
                                })?;
                                AnyResult::Ok(())
                            }.await;
                            match v.map_err(|e| e.downcast::<neolink_core::Error>()) {
                                Err(Ok(neolink_core::Error::UnintelligibleReply{..})) => futures::future::pending().await,
                                Ok(()) => AnyResult::Ok(()),
                                Err(Ok(e)) => Err(e.into()),
                                Err(Err(e)) => Err(e),
                            }?;
                        }
                    }, if config.enable_motion => v,
                    // Handle the SNAP (image preview)
                    v = async {
                        // A sleeping camera's preview comes from the snapshot its wake
                        // reads saved: one per wake, and no second snapshot request
                        // competing with theirs (SPEC P5, P14)
                        let cam_config = camera_snap.config().await?.borrow().clone();
                        if cam_config.sleeps() {
                            let state = StateFile::for_camera(&cam_config);
                            loop {
                                let before = state.load().snapshot_at;
                                camera_snap.next_connection().await?;
                                wait_for_state(|| state.load().snapshot_at != before).await;
                                if let Some((jpeg, _)) = state.snapshot() {
                                    mqtt_snap
                                        .send_message("status/preview", BASE64.encode(jpeg).as_str(), true)
                                        .await
                                        .with_context(|| format!("{}: Failed to publish preview", camera_name))?;
                                }
                            }
                        }
                        let mut wait = IntervalStream::new({
                            let mut i = interval(Duration::from_millis(config.preview_update));
                            i.set_missed_tick_behavior(MissedTickBehavior::Skip);
                            i
                        });
                        let v = async {
                            while wait.next().await.is_some() {
                                let image = camera_snap.run_passive_task(|cam| {
                                    Box::pin(async move {
                                        let image = cam.get_snapshot().await?;
                                        AnyResult::Ok(image)
                                    })
                                }).await;
                                let image = match image {
                                    Err(e) => match e.downcast::<neolink_core::Error>() {
                                        Ok(neolink_core::Error::CameraServiceUnavailable{..}) => {
                                            log::debug!("Image not supported");
                                            futures::future::pending().await
                                        },
                                        Ok(e) => Err(e.into()),
                                        Err(e) => Err(e),
                                    }
                                    n => n,
                                }?;
                                mqtt_snap
                                        .send_message("status/preview", BASE64.encode(image).as_str(), true)
                                        .await
                                        .with_context(|| {
                                            format!("{}: Failed to publish preview", camera_name)
                                        })?;
                            }
                            AnyResult::Ok(())
                        }.await;
                        match v.map_err(|e| e.downcast::<neolink_core::Error>()) {
                            Err(Ok(neolink_core::Error::UnintelligibleReply{..})) => futures::future::pending().await,
                            Ok(()) => AnyResult::Ok(()),
                            Err(Ok(e)) => Err(e.into()),
                            Err(Err(e)) => Err(e),
                        }?;
                        AnyResult::Ok(())
                    }, if config.enable_preview => v,
                    // Handle the battery publish
                    v = async {
                        // A sleeping camera's battery comes from its wake reads (SPEC P7)
                        let cam_config = camera_battery.config().await?.borrow().clone();
                        if cam_config.sleeps() {
                            let state = StateFile::for_camera(&cam_config);
                            loop {
                                let before = state.load().last_battery.map(|b| b.at);
                                camera_battery.next_connection().await?;
                                wait_for_state(|| state.load().last_battery.map(|b| b.at) != before).await;
                                if let Some(b) = state.load().last_battery {
                                    mqtt_battery
                                        .send_message("status/battery_level", format!("{}", b.percent).as_str(), true)
                                        .await
                                        .with_context(|| format!("{}: Failed to publish battery", camera_name))?;
                                }
                            }
                        }
                        let mut wait = IntervalStream::new({
                            let mut i = interval(Duration::from_millis(config.battery_update));
                            i.set_missed_tick_behavior(MissedTickBehavior::Skip);
                            i
                        });

                        let v = async {
                            while wait.next().await.is_some() {
                                let xml = camera_battery.run_passive_task(|cam| {
                                    Box::pin(async move {
                                        let xml = cam.battery_info().await?;
                                        AnyResult::Ok(xml)
                                    })
                                }).await;
                                let xml = match xml {
                                    Err(e) => match e.downcast::<neolink_core::Error>() {
                                        Ok(neolink_core::Error::CameraServiceUnavailable{..}) => {
                                            log::debug!("Battery not supported");
                                            futures::future::pending().await
                                        },
                                        Ok(e) => Err(e.into()),
                                        Err(e) => Err(e),
                                    }
                                    n => n,
                                }?;
                                mqtt_battery
                                        .send_message("status/battery_level", format!("{}", xml.battery_percent).as_str(), true)
                                        .await
                                        .with_context(|| {
                                            format!("{}: Failed to publish battery", camera_name)
                                        })?;
                            }
                            AnyResult::Ok(())
                        }.await;
                        match v.map_err(|e| e.downcast::<neolink_core::Error>()) {
                            Err(Ok(neolink_core::Error::UnintelligibleReply{..})) => futures::future::pending().await,
                            Ok(()) => AnyResult::Ok(()),
                            Err(Ok(e)) => Err(e.into()),
                            Err(Err(e)) => Err(e),
                        }?;
                        AnyResult::Ok(())
                    }, if config.enable_battery => v,
                    // Handle the push notification messages
                    v = async {
                        #[cfg(feature = "pushnoti")]
                        {
                            let mut pn = camera_pn.push_notifications().await?;
                            let mut prev_noti = None;
                            loop {
                                let v = async {
                                    let noti = pn.wait_for(|noti| noti != &prev_noti && noti.is_some()).await.with_context(|| {
                                        format!("{}: PushNoti Watch Dropped", camera_name)
                                    })?.clone();
                                    mqtt_pn.send_message("status/notification", &noti.as_ref().unwrap().message, true).await.with_context(|| {
                                        format!("{}: Failed to publish push notification", camera_name)
                                    })?;
                                    prev_noti = noti;
                                    AnyResult::Ok(())
                                }.await;
                                match v.map_err(|e| e.downcast::<neolink_core::Error>()) {
                                    Err(Ok(neolink_core::Error::UnintelligibleReply{..})) => futures::future::pending().await,
                                    Ok(()) => AnyResult::Ok(()),
                                    Err(Ok(e)) => Err(e.into()),
                                    Err(Err(e)) => Err (e),
                                }?;
                            }
                        }
                        #[cfg(not(feature = "pushnoti"))]
                        unreachable!()
                    }, if cfg!(feature = "pushnoti") => v,
                    // Handle the floodlight task activation
                    v = async {
                        let flt_status = camera_floodlight_tasks.run_passive_task(|cam| Box::pin(async move {
                            Ok(cam.is_flightlight_tasks_enabled().await?)
                        })).await;
                        if flt_status.is_err() {
                            // Assume floodlight unsupported
                            futures::future::pending::<()>().await;
                        }
                        let flt_status = flt_status.unwrap();
                        let flt_status_txt = match flt_status {
                            true => "on".to_string(),
                            false => "off".to_string(),
                        };
                        mqtt_floodlight_tasks.send_message("status/floodlight_tasks", &flt_status_txt, true).await.with_context(|| {
                            format!("{}: Failed to publish floodlight task notification", camera_name)
                        })?;

                        let mut wait = IntervalStream::new({
                            let mut i = interval(Duration::from_millis(config.floodlight_update));
                            i.set_missed_tick_behavior(MissedTickBehavior::Skip);
                            i
                        });
                        while wait.next().await.is_some() {
                            let flt_status = camera_floodlight_tasks.run_passive_task(|cam| Box::pin(async move {
                                Ok(cam.is_flightlight_tasks_enabled().await?)
                            })).await;
                            if let Ok(flt_status) = flt_status {
                                let flt_status_txt = match flt_status {
                                    true => "on".to_string(),
                                    false => "off".to_string(),
                                };
                                mqtt_floodlight_tasks.send_message("status/floodlight_tasks", &flt_status_txt, true).await.with_context(|| {
                                    format!("{}: Failed to publish floodlight task notification", camera_name)
                                })?;
                            }
                        }
                        AnyResult::Ok(())
                    }, if config.enable_floodlight => v,
                }?;
                AnyResult::Ok(())
            } => v,
        };
    };

    drop(drop_cancel);
    r?;
    Ok(())
}

async fn handle_mqtt_message(
    msg: MqttReply,
    mqtt: &MqttInstance,
    camera: &NeoInstance,
    power: &CamPower,
) -> Result<()> {
    // The ledger names the topic that woke the camera (SPEC P6)
    let reason = format!("mqtt:{}", msg.topic);
    let reason = reason.as_str();
    match msg.as_ref() {
        MqttReplyRef {
            topic: _,
            message: "OK",
        }
        | MqttReplyRef {
            topic: _,
            message: "FAIL",
        } => {
            // Do nothing for the success/fail replies
        }
        MqttReplyRef { topic: _, message }
            if message.starts_with("FAIL:") || message.starts_with("OK:") =>
        {
            // Do nothing for the success/fail replies
        }
        MqttReplyRef {
            topic: "control/floodlight",
            message: "on",
        } => {
            let res = camera
                .run_task_for(reason, |cam| {
                    Box::pin(async move {
                        cam.set_floodlight_manual(true, 180).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn on the floodlight light: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/floodlight", &reply, false)
                .await
                .with_context(|| "Failed to publish camera status light on")?;
        }
        MqttReplyRef {
            topic: "control/floodlight",
            message: "off",
        } => {
            let res = camera
                .run_task_for(reason, |cam| {
                    Box::pin(async move {
                        cam.set_floodlight_manual(false, 180).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn off the floodlight light: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/floodlight", &reply, false)
                .await
                .with_context(|| "Failed to publish camera status light off")?;
        }
        MqttReplyRef {
            topic: "control/led",
            message: "on",
        } => {
            let res = camera
                .run_task_for(reason, |cam| {
                    Box::pin(async move {
                        cam.led_light_set(true).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn on the led: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/led", &reply, false)
                .await
                .with_context(|| "Failed to publish led on")?;
        }
        MqttReplyRef {
            topic: "control/led",
            message: "off",
        } => {
            let res = camera
                .run_task_for(reason, |cam| {
                    Box::pin(async move {
                        cam.led_light_set(false).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn off the led: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/led", &reply, false)
                .await
                .with_context(|| "Failed to publish led off")?;
        }
        MqttReplyRef {
            topic: "control/ir",
            message: "on",
        } => {
            let res = camera
                .run_task_for(reason, |cam| {
                    Box::pin(async move {
                        cam.irled_light_set(LightState::On).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn on the ir: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/ir", &reply, false)
                .await
                .with_context(|| "Failed to publish ir on")?;
        }
        MqttReplyRef {
            topic: "control/ir",
            message: "off",
        } => {
            let res = camera
                .run_task_for(reason, |cam| {
                    Box::pin(async move {
                        cam.irled_light_set(LightState::Off).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn off the ir: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/ir", &reply, false)
                .await
                .with_context(|| "Failed to publish ir off")?;
        }
        MqttReplyRef {
            topic: "control/ir",
            message: "auto",
        } => {
            let res = camera
                .run_task_for(reason, |cam| {
                    Box::pin(async move {
                        cam.irled_light_set(LightState::Auto).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn set to auto on the led: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/ir", &reply, false)
                .await
                .with_context(|| "Failed to publish ir auto")?;
        }
        MqttReplyRef {
            topic: "control/reboot",
            ..
        } => {
            let res = camera
                .run_task_for(reason, |cam| {
                    Box::pin(async move {
                        cam.reboot().await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to reboot the camera: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/ir", &reply, false)
                .await
                .with_context(|| "Failed to publish reboot on the camera")?;
        }
        MqttReplyRef {
            topic: "control/zoom",
            message,
        } => {
            let reply = if let Ok(amount) = message.parse::<f32>() {
                if let Err(e) = camera
                    .run_task_for(reason, |cam| {
                        Box::pin(async move {
                            cam.zoom_to((amount * 1000.0) as u32).await?;
                            AnyResult::Ok(())
                        })
                    })
                    .await
                {
                    error!("Failed to send PTZ: {:?}", e);
                    format!("FAIL: {e:?}")
                } else {
                    "OK".to_string()
                }
            } else {
                "FAIL: Could not convert message to number".to_string()
            };

            mqtt.send_message("control/zoom", &reply, false)
                .await
                .with_context(|| "Failed to publish zoom on the camera")?;
        }
        MqttReplyRef {
            topic: "control/ptz",
            message,
        }
        | MqttReplyRef {
            topic: "control/pt",
            message,
        } => {
            let lowercase_message = message.to_lowercase();
            let mut words = lowercase_message.split_whitespace();
            let reply = if let Some(direction_txt) = words.next() {
                // Target amount to move
                let speed = 32f32;
                let amount = words.next().unwrap_or("32.0");

                if let Ok(amount) = amount.parse::<f32>() {
                    let seconds = amount / speed;
                    // range checking on seconds so that you can't sleep for 3.4E+38 seconds
                    let seconds = match seconds {
                        x if (0.0..10.0).contains(&x) => Some(seconds),
                        _ => {
                            error!("seconds was not a valid number (out of range)");
                            None
                        }
                    };

                    let bc_direction = match direction_txt {
                        "up" => Some(BcDirection::Up),
                        "down" => Some(BcDirection::Down),
                        "left" => Some(BcDirection::Left),
                        "right" => Some(BcDirection::Right),
                        n => {
                            error!("Unrecognized PTZ direction \"{}\"", n);
                            None
                        }
                    };

                    if let (Some(seconds), Some(bc_direction)) = (seconds, bc_direction) {
                        // On drop send the stop command again just to make sure it stops
                        let _drop_command = camera.clone().drop_command(
                            move |cam| {
                                Box::pin(async move {
                                    cam.send_ptz(BcDirection::Stop, speed).await?;
                                    AnyResult::Ok(())
                                })
                            },
                            Duration::from_millis(100),
                        );
                        if let Err(e) = camera
                            .run_task_for(reason, |cam| {
                                Box::pin(async move {
                                    cam.send_ptz(bc_direction, speed).await?;
                                    sleep(Duration::from_secs_f32(seconds)).await;
                                    cam.send_ptz(BcDirection::Stop, speed).await?;
                                    AnyResult::Ok(())
                                })
                            })
                            .await
                        {
                            error!("Failed to send PTZ: {:?}", e);
                            "FAIL"
                        } else {
                            "OK"
                        }
                    } else {
                        "FAIL"
                    }
                } else {
                    error!("No PTZ speed as a valid number");
                    "FAIL"
                }
            } else {
                error!("No PTZ Direction given. Please add up/down/left/right/in/out");
                "FAIL"
            }
            .to_string();

            mqtt.send_message("control/ptz", &reply, false)
                .await
                .with_context(|| "Failed to publish ptz on the camera")?;
        }
        MqttReplyRef {
            topic: "control/ptz/preset",
            message,
        } => {
            let reply = if let Ok(id) = message.parse::<u8>() {
                let res = camera
                    .run_task_for(reason, |cam| {
                        Box::pin(async move {
                            cam.moveto_ptz_preset(id).await?;
                            AnyResult::Ok(())
                        })
                    })
                    .await;
                if res.is_err() {
                    error!("Failed to move to ptz preset: {:?}", res.err());
                    "FAIL"
                } else {
                    "OK"
                }
            } else {
                error!("PTZ preset was not a valid number");
                "FAIL"
            }
            .to_string();
            mqtt.send_message("control/ir", &reply, false)
                .await
                .with_context(|| "Failed to publish ptz move")?;
        }
        MqttReplyRef {
            topic: "control/ptz/assign",
            message,
        } => {
            let mut words = message.split_whitespace();
            let id = words.next();
            let name = words.next();

            let reply = if let (Some(Ok(id)), Some(name)) = (id.map(|id| id.parse::<u8>()), name) {
                let name = name.to_owned();
                let res = camera
                    .run_task_for(reason, |cam| {
                        let name = name.clone();
                        Box::pin(async move {
                            cam.set_ptz_preset(id, name).await?;
                            AnyResult::Ok(())
                        })
                    })
                    .await;
                if res.is_err() {
                    error!("Failed to assign ptz preset: {:?}", res.err());
                    "FAIL"
                } else {
                    "OK"
                }
            } else if let (Some(Err(_)), _) = (id.map(|id| id.parse::<u8>()), name) {
                error!("PTZ preset was not a valid number");
                "FAIL"
            } else if let (_, None) = (id.map(|id| id.parse::<u8>()), name) {
                error!("PTZ preset was not given a name");
                "FAIL"
            } else {
                "FAIL"
            }
            .to_string();
            mqtt.send_message("control/ir", &reply, false)
                .await
                .with_context(|| "Failed to publish ptz move")?;
        }
        MqttReplyRef {
            topic: "control/pir",
            message: "on",
        } => {
            let res = camera
                .run_task_for(reason, |cam| {
                    Box::pin(async move {
                        cam.pir_set(true).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn on the pir: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/pir", &reply, false)
                .await
                .with_context(|| "Failed to publish pir on")?;
        }
        MqttReplyRef {
            topic: "control/pir",
            message: "off",
        } => {
            let res = camera
                .run_task_for(reason, |cam| {
                    Box::pin(async move {
                        cam.pir_set(false).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn off the pir: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/pir", &reply, false)
                .await
                .with_context(|| "Failed to publish pir off")?;
        }
        MqttReplyRef {
            topic: "control/wakeup",
            message,
        } => {
            let name = camera.config().await?.borrow().name.clone();
            let reply = power.wakeup(&name, message);
            mqtt.send_message("control/wakeup", &reply, false)
                .await
                .with_context(|| "Failed to publish wakeup")?;
        }
        MqttReplyRef {
            topic: "control/floodlight_tasks",
            message,
        } => {
            let state = match message.to_lowercase().as_ref() {
                "on" => Ok(true),
                "off" => Ok(false),
                n => match n.parse::<bool>() {
                    Ok(state) => Ok(state),
                    Err(e) => AnyResult::Err(e.into()),
                },
            };

            let reply = match state {
                Ok(state) => {
                    if let Err(e) = camera
                        .run_task_for(reason, |cam| {
                            Box::pin(async move {
                                cam.flightlight_tasks_enable(state).await?;
                                AnyResult::Ok(())
                            })
                        })
                        .await
                    {
                        format!("FAIL: {e:?}")
                    } else {
                        "OK".to_string()
                    }
                }
                Err(e) => format!("FAIL: Could not parse message to {e:?}"),
            };

            mqtt.send_message("control/floodlight_tasks", &reply, false)
                .await
                .with_context(|| "Failed to publish floodlight_tasks")?;
        }
        MqttReplyRef {
            topic: "control/siren",
            message: "on",
        } => {
            let res = camera
                .run_task_for(reason, |cam| {
                    Box::pin(async move {
                        cam.siren().await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if let Err(e) = res {
                error!("Failed to trigger siren: {:?}", e);
                format!("FAIL: {e:?}")
            } else {
                "OK".to_string()
            };

            mqtt.send_message("control/siren", &reply, false)
                .await
                .with_context(|| "Failed to publish siren")?;
        }
        MqttReplyRef {
            topic: "query/battery",
            message,
        } => {
            let reply = query_battery(camera, mqtt, power, wants_fresh(message)).await?;
            mqtt.send_message("query/battery", &reply, false)
                .await
                .with_context(|| "Failed to publish battery query")?;
        }
        MqttReplyRef {
            topic: "query/pir",
            message,
        } => {
            let reply = match live_query(camera, power, "query/pir", wants_fresh(message), |cam| {
                Box::pin(async move { Ok(cam.get_pirstate().await?) })
            })
            .await
            {
                Answer::Value(xml) => publish_xml(mqtt, "status/pir", &xml).await?,
                Answer::Reply(r) => r,
            };
            mqtt.send_message("query/pir", &reply, false)
                .await
                .with_context(|| "Failed to publish pir query")?;
        }
        MqttReplyRef {
            topic: "query/ptz/preset",
            message,
        } => {
            let reply = match live_query(
                camera,
                power,
                "query/ptz/preset",
                wants_fresh(message),
                |cam| Box::pin(async move { Ok(cam.get_ptz_preset().await?) }),
            )
            .await
            {
                Answer::Value(xml) => publish_xml(mqtt, "status/ptz", &xml).await?,
                Answer::Reply(r) => r,
            };
            mqtt.send_message("query/ptz", &reply, false)
                .await
                .with_context(|| "Failed to publish ptz query")?;
        }
        MqttReplyRef {
            topic: "query/preview",
            message,
        } => {
            let reply = query_preview(camera, mqtt, power, wants_fresh(message)).await?;
            mqtt.send_message("query/preview", &reply, false)
                .await
                .with_context(|| "Failed to publish preview query")?;
        }
        _ => {}
    }
    Ok(())
}

/// What a query came to
enum Answer<T> {
    /// The camera's answer, to publish
    Value(T),
    /// No answer: the reply to send instead
    Reply(String),
}

/// A live read for a query. On a sleeping camera it never wakes it without `fresh`: it
/// reads only if the camera is awake anyway, without keeping it awake. Concurrent `fresh`
/// queries on a topic share one wake (SPEC P11)
async fn live_query<T, F>(
    camera: &NeoInstance,
    power: &CamPower,
    topic: &str,
    fresh: bool,
    task: F,
) -> Answer<T>
where
    F: for<'a> Fn(
        &'a neolink_core::bc_protocol::BcCamera,
    )
        -> std::pin::Pin<Box<dyn futures::Future<Output = AnyResult<T>> + Send + 'a>>,
{
    let sleeps = match camera.config().await {
        Ok(c) => c.borrow().sleeps(),
        Err(e) => return Answer::Reply(format!("FAIL: {e:#}")),
    };
    let reason = format!("mqtt:{topic}");
    let awake = camera.camera().borrow().upgrade().is_some();
    let result = if !sleeps {
        camera.run_task_for(&reason, task).await
    } else if !fresh {
        if !awake {
            return Answer::Reply(format!(
                "FAIL: the camera is asleep and nothing saved answers {topic}; send `fresh` to wake it"
            ));
        }
        // Awake for something else: read without holding it awake
        match tokio::time::timeout(Duration::from_secs(10), camera.run_passive_task(task)).await {
            Ok(r) => r,
            Err(_) => Err(anyhow!("the camera went to sleep before answering")),
        }
    } else {
        let Some(_in_flight) = power.begin(topic) else {
            return Answer::Reply(
                "OK: a fresh query is already waking the camera; its answer follows".to_string(),
            );
        };
        camera.run_task_for(&reason, task).await
    };
    match result {
        Ok(v) => Answer::Value(v),
        Err(e) => {
            error!("{topic} failed: {e:?}");
            Answer::Reply(format!("FAIL: {e:#}"))
        }
    }
}

/// Publishes `value` as XML on `topic`; returns the reply
async fn publish_xml<T: serde::Serialize>(
    mqtt: &MqttInstance,
    topic: &str,
    value: &T,
) -> Result<String> {
    let mut buf = bytes::BytesMut::new();
    let xml = match quick_xml::se::to_writer(&mut buf, value)
        .map_err(anyhow::Error::from)
        .and_then(|_| Ok(String::from_utf8(buf.to_vec())?))
    {
        Ok(xml) => xml,
        Err(e) => {
            error!("Failed to serialise {topic}: {e:?}");
            return Ok("FAIL: could not serialise the reply".to_string());
        }
    };
    mqtt.send_message(topic, &xml, false)
        .await
        .with_context(|| format!("Failed to publish {topic}"))?;
    Ok("OK".to_string())
}

/// `query/battery`: a sleeping camera answers from its last wake (SPEC P7)
async fn query_battery(
    camera: &NeoInstance,
    mqtt: &MqttInstance,
    power: &CamPower,
    fresh: bool,
) -> Result<String> {
    let config = camera.config().await?.borrow().clone();
    let state = StateFile::for_camera(&config);
    if config.sleeps() {
        let saved = state.load().last_battery;
        if let Some(b) = saved.filter(|b| !fresh || b.age() <= FRESH_MAX_AGE_S) {
            let reply = publish_xml(mqtt, "status/battery", &b.to_info(config.channel_id)).await?;
            return Ok(saved_reply(reply, b.age(), fresh));
        }
    }
    let info = match live_query(camera, power, "query/battery", fresh, |cam| {
        Box::pin(async move { Ok(cam.battery_info().await?) })
    })
    .await
    {
        Answer::Value(info) => info,
        Answer::Reply(r) => return Ok(r),
    };
    if config.sleeps() {
        if let Err(e) = state.update(|s| {
            s.battery = Some(true);
            s.last_battery = Some(BatteryReading::now(&info));
        }) {
            warn!("{}: could not save the battery reading: {e:?}", config.name);
        }
    }
    publish_xml(mqtt, "status/battery", &info).await
}

/// `query/preview`: a sleeping camera answers with the snapshot of its last wake (SPEC P5)
async fn query_preview(
    camera: &NeoInstance,
    mqtt: &MqttInstance,
    power: &CamPower,
    fresh: bool,
) -> Result<String> {
    let config = camera.config().await?.borrow().clone();
    let state = StateFile::for_camera(&config);
    let publish = |jpeg: Vec<u8>| async move {
        mqtt.send_message("status/preview", BASE64.encode(jpeg).as_str(), true)
            .await
            .with_context(|| "Failed to publish preview")
    };
    if config.sleeps() {
        if let Some((jpeg, at)) = state.snapshot() {
            let age = unix_now().saturating_sub(at);
            if !fresh || age <= FRESH_MAX_AGE_S {
                publish(jpeg).await?;
                return Ok(saved_reply("OK".to_string(), age, fresh));
            }
        }
    }
    let jpeg = match live_query(camera, power, "query/preview", fresh, |cam| {
        Box::pin(async move { Ok(cam.get_snapshot().await?) })
    })
    .await
    {
        Answer::Value(jpeg) => jpeg,
        Answer::Reply(r) => return Ok(r),
    };
    if config.sleeps() {
        if let Err(e) = state.save_snapshot(&jpeg) {
            warn!("{}: could not save the snapshot: {e:?}", config.name);
        }
    }
    publish(jpeg).await?;
    Ok("OK".to_string())
}

/// The reply for an answer from the state file: its age, and how to get a new one
fn saved_reply(published: String, age: u64, fresh: bool) -> String {
    if published != "OK" {
        published
    } else if fresh {
        format!(
            "OK: saved {age} s ago (fresh queries reuse a reading up to {FRESH_MAX_AGE_S} s old)"
        )
    } else {
        format!("OK: saved {age} s ago (send `fresh` to wake the camera for a new one)")
    }
}

/// Wait up to 60 s for the wake reads to update the state file
async fn wait_for_state(updated: impl Fn() -> bool) {
    for _ in 0..120 {
        if updated() {
            return;
        }
        sleep(Duration::from_millis(500)).await;
    }
}
