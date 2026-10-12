//! MQTT requests that could wake or pin a sleeping camera
//!
//! Every wake costs a battery camera charge, so MQTT clients (Home Assistant automations,
//! scripts) only wake one with bounded, explicit intent:
//!
//! - `control/wakeup <minutes>`: whole minutes, capped at [`WAKEUP_MAX_MIN`]; a new wakeup
//!   replaces the pending one, `0` cancels it, and it ends with the camera's MQTT listener.
//! - `query/battery` and `query/preview` answer from the last wake (the state file) and
//!   wake the camera only for the payload `fresh`, unless a reading at most
//!   [`FRESH_MAX_AGE_S`] old answers it. Other queries have nothing saved and need `fresh`.
//!   Concurrent `fresh` queries on a topic share one wake.
//!
//! Always-on cameras are connected anyway and answer queries live.

use anyhow::Result;
use std::collections::HashSet;
use std::sync::Mutex;
use tokio::sync::watch;
use tokio::time::{Duration, Instant};

use crate::common::NeoInstance;

/// Longest `control/wakeup`, in minutes: SPEC P8 warns about battery cameras streaming for
/// longer than this, and an automation that needs more can renew it
pub(crate) const WAKEUP_MAX_MIN: u64 = 30;

/// A `fresh` query is answered from a reading at most this old without waking the camera:
/// it rate-limits automations that ask for fresh values in a loop
pub(crate) const FRESH_MAX_AGE_S: u64 = 60;

/// The payload that asks a query to wake a sleeping camera
pub(crate) fn wants_fresh(message: &str) -> bool {
    message.trim().eq_ignore_ascii_case("fresh")
}

/// The MQTT power state of one camera, shared by its message handlers
pub(crate) struct CamPower {
    /// The pending `control/wakeup`: when it ends
    wakeup: watch::Sender<Option<Instant>>,
    /// Queries waking the camera right now, by topic
    in_flight: Mutex<HashSet<String>>,
}

impl CamPower {
    pub(crate) fn new() -> Self {
        Self {
            wakeup: watch::channel(None).0,
            in_flight: Default::default(),
        }
    }

    /// Handles `control/wakeup`; returns the reply
    pub(crate) fn wakeup(&self, name: &str, message: &str) -> String {
        let minutes = match message.trim().parse::<u64>() {
            Ok(m) => m,
            Err(e) => {
                let shown: String = message.chars().take(40).collect();
                log::warn!("{name}: MQTT control/wakeup: rejected {shown:?}: {e}");
                return format!(
                    "FAIL: expected whole minutes, 0 to {WAKEUP_MAX_MIN}, got {shown:?} ({e})"
                );
            }
        };
        if minutes == 0 {
            if self.wakeup.send_replace(None).is_some() {
                log::info!("{name}: MQTT control/wakeup: cancelled");
            }
            return "OK: wakeup cancelled".to_string();
        }
        let granted = minutes.min(WAKEUP_MAX_MIN);
        // A new wakeup replaces the pending one: they never stack
        self.wakeup
            .send_replace(Some(Instant::now() + Duration::from_secs(granted * 60)));
        if granted < minutes {
            log::warn!(
                "{name}: MQTT control/wakeup: keeping the camera awake for {granted} min, the maximum (asked for {minutes})"
            );
            format!("OK: awake for {granted} min, the maximum (asked for {minutes})")
        } else {
            log::info!("{name}: MQTT control/wakeup: keeping the camera awake for {granted} min");
            format!("OK: awake for {granted} min")
        }
    }

    /// Holds the camera awake while a wakeup is pending. Runs for as long as the camera's
    /// MQTT listener: when that stops (camera removed, MQTT reconfigured, shutdown) the
    /// permit goes with it
    pub(crate) async fn run_wakeups(&self, camera: &NeoInstance, name: &str) -> Result<()> {
        let mut rx = self.wakeup.subscribe();
        loop {
            let pending = *rx.borrow_and_update();
            match pending {
                Some(until) if until > Instant::now() => {
                    let _permit = camera.permit("mqtt:control/wakeup").await?;
                    tokio::select! {
                        _ = tokio::time::sleep_until(until) => {
                            log::info!("{name}: MQTT control/wakeup ended");
                            self.wakeup.send_if_modified(|w| {
                                let ended = *w == Some(until);
                                if ended {
                                    *w = None;
                                }
                                ended
                            });
                        }
                        r = rx.changed() => r?,
                    }
                }
                _ => rx.changed().await?,
            }
        }
    }

    /// Marks `topic` as waking the camera; `None` if a query on it already is (P11)
    pub(crate) fn begin(&self, topic: &str) -> Option<InFlight<'_>> {
        let mut set = self.in_flight.lock().unwrap();
        if !set.insert(topic.to_string()) {
            return None;
        }
        Some(InFlight {
            power: self,
            topic: topic.to_string(),
        })
    }
}

/// A query waking the camera; dropping it ends it
pub(crate) struct InFlight<'a> {
    power: &'a CamPower,
    topic: String,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.power.in_flight.lock().unwrap().remove(&self.topic);
    }
}
