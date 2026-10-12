//! MQTT against the fake camera without a broker: what wakes a sleeping camera
//! (battery safety review). Each test counts the fake camera's logins

use super::mqttc::{MqttInstance, MqttReply};
use super::*;
use crate::common::tests::{config_for, new_neocam, wait_stats};
use crate::common::{unix_now, BatteryReading, NeoCam};
use neolink_core::test_util::{FakeCamera, FakeCameraConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

const MSG_ID_BATTERY_INFO: u32 = 253;
const MSG_ID_MOTION_REQUEST: u32 = 31;

struct Harness {
    cam: FakeCamera,
    neocam: NeoCam,
    incoming: broadcast::Sender<MqttReply>,
    published_rx: mpsc::Receiver<(String, String, bool)>,
    /// Everything published so far: (topic, message, retained)
    published: Vec<(String, String, bool)>,
    listener: tokio::task::JoinHandle<Result<()>>,
}

impl Harness {
    async fn start(fake: FakeCameraConfig, extra: &str) -> Self {
        let cam = FakeCamera::start(fake).await.unwrap();
        let neocam = new_neocam(config_for(&cam, extra)).await;
        let (mqtt, incoming, published_rx) = MqttInstance::for_test("fake");
        let camera = neocam.subscribe().await.unwrap();
        let listener = tokio::spawn(listen_on_camera(camera, mqtt));
        let mut h = Self {
            cam,
            neocam,
            incoming,
            published_rx,
            published: vec![],
            listener,
        };
        // Started once the initial states are out
        h.wait_published(10, |(t, _, _)| t == "neolink/fake/status/motion")
            .await;
        h
    }

    fn send(&self, topic: &str, message: &str) {
        self.incoming
            .send(MqttReply {
                topic: format!("fake/{topic}"),
                message: Arc::new(message.to_string()),
            })
            .unwrap();
    }

    fn drain(&mut self) {
        while let Ok(p) = self.published_rx.try_recv() {
            self.published.push(p);
        }
    }

    /// Wait until something matching `pred` was published; returns it
    async fn wait_published(
        &mut self,
        secs: u64,
        pred: impl Fn(&(String, String, bool)) -> bool,
    ) -> (String, String, bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            self.drain();
            if let Some(p) = self.published.iter().find(|p| pred(p)) {
                return p.clone();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "not published; got {:?}",
                self.published
                    .iter()
                    .map(|(t, m, _)| (t, m.chars().take(80).collect::<String>()))
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The next reply neolink publishes on `topic` (its OK/FAIL answer)
    async fn reply(&mut self, topic: &str) -> String {
        let full = format!("neolink/fake/{topic}");
        let (t, m, _) = self
            .wait_published(70, |(t, m, _)| {
                *t == full && (m.starts_with("OK") || m.starts_with("FAIL"))
            })
            .await;
        // Consume it, so the next call waits for the next reply
        let i = self
            .published
            .iter()
            .position(|(pt, pm, _)| *pt == t && *pm == m)
            .unwrap();
        self.published.remove(i);
        m
    }

    async fn wait_use_count(&self, n: u32, secs: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        while self.neocam.use_count() != n {
            assert!(
                tokio::time::Instant::now() < deadline,
                "use count {} (want {}): {:?}",
                self.neocam.use_count(),
                n,
                self.neocam.permit_reasons()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn saved_battery(percent: u32, age_s: u64) -> BatteryReading {
    BatteryReading {
        percent,
        voltage: 3900,
        current: 0,
        temperature: 20,
        charge_status: "none".to_string(),
        adapter_status: "solarPanel".to_string(),
        at: unix_now() - age_s,
    }
}

// --- control/wakeup

/// Garbage and out-of-range payloads are rejected with an error reply and wake nothing
#[tokio::test]
async fn test_wakeup_rejects_garbage() {
    let mut h = Harness::start(FakeCameraConfig::default(), "").await;
    for bad in [
        "abc",
        "-1",
        "1e30",
        "1.5",
        "",
        "18446744073709551616",
        "99999999999999999999999",
    ] {
        h.send("control/wakeup", bad);
        let r = h.reply("control/wakeup").await;
        assert!(r.starts_with("FAIL"), "{:?} -> {}", bad, r);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(h.neocam.use_count(), 0);
    assert_eq!(h.cam.stats().connections, 0);
}

/// A huge but valid number is capped at WAKEUP_MAX_MIN; before, `minutes * 60` overflowed
/// (a panic in debug builds, a wrapped duration in release) or pinned the camera for ever
#[tokio::test]
async fn test_wakeup_is_capped() {
    let mut h = Harness::start(FakeCameraConfig::default(), "").await;
    h.send("control/wakeup", "999999999999999999");
    let r = h.reply("control/wakeup").await;
    assert!(r.starts_with("OK") && r.contains("30 min"), "{}", r);
    h.wait_use_count(1, 5).await;
    wait_stats(&h.cam, 10, |s| s.logins == 1).await;
}

/// Repeated wakeups replace each other instead of stacking permits, `0` cancels, and the
/// wake is released after the grace
#[tokio::test]
async fn test_wakeup_replaces_and_cancels() {
    let mut h = Harness::start(FakeCameraConfig::default(), "idle_grace_s = 1").await;
    for _ in 0..5 {
        h.send("control/wakeup", "5");
        assert!(h.reply("control/wakeup").await.starts_with("OK"));
    }
    wait_stats(&h.cam, 10, |s| s.logins == 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(h.neocam.use_count(), 1, "{:?}", h.neocam.permit_reasons());
    assert_eq!(
        h.neocam.permit_reasons(),
        vec!["mqtt:control/wakeup".to_string()]
    );
    h.send("control/wakeup", "0");
    assert!(h.reply("control/wakeup").await.starts_with("OK"));
    h.wait_use_count(0, 5).await;
    wait_stats(&h.cam, 10, |s| s.open_connections == 0 && s.logouts == 1).await;
    assert_eq!(h.cam.stats().logins, 1);
    let wakes = h.neocam.ledger().lock().unwrap().wakes();
    assert_eq!(wakes.len(), 1);
    assert_eq!(wakes[0].reason, "mqtt:control/wakeup");
}

/// The wakeup ends with the camera's MQTT listener (camera removed, MQTT reconfigured,
/// shutdown); before, a detached task held the permit for the full duration
#[tokio::test]
async fn test_wakeup_ends_with_listener() {
    let mut h = Harness::start(FakeCameraConfig::default(), "idle_grace_s = 1").await;
    h.send("control/wakeup", "30");
    assert!(h.reply("control/wakeup").await.starts_with("OK"));
    h.wait_use_count(1, 5).await;
    h.listener.abort();
    h.wait_use_count(0, 5).await;
    wait_stats(&h.cam, 10, |s| s.open_connections == 0).await;
}

// --- query/battery

/// P7: a sleeping camera's battery query is answered from the last wake, with its age
#[tokio::test]
async fn test_battery_query_answers_from_state() {
    let mut h = Harness::start(FakeCameraConfig::default(), "").await;
    h.neocam
        .state()
        .update(|s| s.last_battery = Some(saved_battery(55, 120)))
        .unwrap();
    for _ in 0..100 {
        h.send("query/battery", "");
    }
    for _ in 0..100 {
        let r = h.reply("query/battery").await;
        assert!(r.starts_with("OK") && r.contains("s ago"), "{}", r);
    }
    let (_, xml, _) = h
        .wait_published(5, |(t, _, _)| t == "neolink/fake/status/battery")
        .await;
    assert!(xml.contains("<batteryPercent>55</batteryPercent>"), "{}", xml);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(h.cam.stats().connections, 0, "a battery query woke the camera");
}

/// With nothing saved yet the query fails rather than waking the camera
#[tokio::test]
async fn test_battery_query_without_state_does_not_wake() {
    let mut h = Harness::start(FakeCameraConfig::default(), "").await;
    h.send("query/battery", "");
    let r = h.reply("query/battery").await;
    assert!(r.starts_with("FAIL") && r.contains("fresh"), "{}", r);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(h.cam.stats().connections, 0);
}

/// P11: concurrent `fresh` queries make one wake and one read; the ledger names the topic
#[tokio::test]
async fn test_fresh_battery_queries_coalesce() {
    let mut h = Harness::start(FakeCameraConfig::default(), "idle_grace_s = 1").await;
    for _ in 0..20 {
        h.send("query/battery", "fresh");
    }
    for _ in 0..20 {
        let r = h.reply("query/battery").await;
        assert!(r.starts_with("OK"), "{}", r);
    }
    let (_, xml, _) = h
        .wait_published(5, |(t, _, _)| t == "neolink/fake/status/battery")
        .await;
    assert!(xml.contains("<batteryPercent>80</batteryPercent>"), "{}", xml);
    assert_eq!(h.cam.stats().logins, 1);
    // One read for the queries; the wake's own reads add at most one more
    let reads = h
        .cam
        .stats()
        .messages
        .get(&MSG_ID_BATTERY_INFO)
        .copied()
        .unwrap_or(0);
    assert!(reads <= 2, "{} battery reads", reads);
    let wakes = h.neocam.ledger().lock().unwrap().wakes();
    assert_eq!(wakes.len(), 1);
    assert_eq!(wakes[0].reason, "mqtt:query/battery");
    // A fresh query right after is answered by that reading
    wait_stats(&h.cam, 10, |s| s.open_connections == 0).await;
    h.send("query/battery", "fresh");
    assert!(h.reply("query/battery").await.starts_with("OK"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(h.cam.stats().logins, 1);
}

// --- query/preview and other queries

/// P5: a sleeping camera's preview query serves the snapshot from its last wake
#[tokio::test]
async fn test_preview_query_answers_from_state() {
    let mut h = Harness::start(FakeCameraConfig::default(), "").await;
    h.neocam.state().save_snapshot(b"saved jpeg").unwrap();
    for _ in 0..20 {
        h.send("query/preview", "");
    }
    for _ in 0..20 {
        assert!(h.reply("query/preview").await.starts_with("OK"));
    }
    let (_, b64, _) = h
        .wait_published(5, |(t, _, _)| t == "neolink/fake/status/preview")
        .await;
    assert_eq!(BASE64.decode(b64).unwrap(), b"saved jpeg");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(h.cam.stats().connections, 0);
}

/// Queries without a saved answer (PIR, PTZ presets) don't wake a sleeping camera unless
/// asked for with `fresh`
#[tokio::test]
async fn test_other_queries_need_fresh() {
    let mut h = Harness::start(FakeCameraConfig::default(), "").await;
    h.send("query/pir", "");
    let r = h.reply("query/pir").await;
    assert!(r.starts_with("FAIL") && r.contains("fresh"), "{}", r);
    h.send("query/ptz/preset", "");
    let r = h.reply("query/ptz").await;
    assert!(r.starts_with("FAIL") && r.contains("fresh"), "{}", r);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(h.cam.stats().connections, 0);
}

/// An always-on camera answers queries live, as before
#[tokio::test]
async fn test_always_on_queries_are_live() {
    let mut h = Harness::start(FakeCameraConfig::default(), "battery = false").await;
    wait_stats(&h.cam, 10, |s| s.logins == 1).await;
    h.send("query/battery", "");
    assert_eq!(h.reply("query/battery").await, "OK");
    let (_, xml, _) = h
        .wait_published(5, |(t, _, _)| t == "neolink/fake/status/battery")
        .await;
    assert!(xml.contains("<batteryPercent>80</batteryPercent>"), "{}", xml);
}

// --- status topics

/// Motion that was `on` when the camera went to sleep (or dropped) is published `off`;
/// before, it stayed `on` until the next motion
#[tokio::test]
async fn test_motion_off_after_disconnect() {
    let mut h = Harness::start(FakeCameraConfig::default(), "idle_grace_s = 1").await;
    let instance = h.neocam.subscribe().await.unwrap();
    let permit = instance.permit("test:wake").await.unwrap();
    wait_stats(&h.cam, 10, |s| s.messages.contains_key(&MSG_ID_MOTION_REQUEST)).await;
    h.cam.motion(true);
    h.wait_published(5, |(t, m, _)| t == "neolink/fake/status/motion" && m == "on")
        .await;
    drop(permit);
    h.cam.drop_connections();
    h.wait_published(10, |(t, m, _)| t == "neolink/fake/status/motion" && m == "off")
        .await;
}

/// P6: the wake ledger is published (retained) on every wake and sleep
#[tokio::test]
async fn test_ledger_published() {
    let mut h = Harness::start(FakeCameraConfig::default(), "idle_grace_s = 1").await;
    h.send("control/wakeup", "1");
    assert!(h.reply("control/wakeup").await.starts_with("OK"));
    let (_, json, retained) = h
        .wait_published(10, |(t, m, _)| {
            t == "neolink/fake/status/wakes" && m.contains("\"awake\":true")
        })
        .await;
    assert!(retained);
    assert!(json.contains("\"reason\":\"mqtt:control/wakeup\""), "{}", json);
    h.send("control/wakeup", "0");
    h.wait_published(10, |(t, m, _)| {
        t == "neolink/fake/status/wakes" && m.contains("\"awake\":false")
    })
    .await;
}
