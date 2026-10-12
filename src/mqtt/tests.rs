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
    assert!(
        xml.contains("<batteryPercent>55</batteryPercent>"),
        "{}",
        xml
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        h.cam.stats().connections,
        0,
        "a battery query woke the camera"
    );
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
    assert!(
        xml.contains("<batteryPercent>80</batteryPercent>"),
        "{}",
        xml
    );
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
    assert!(
        xml.contains("<batteryPercent>80</batteryPercent>"),
        "{}",
        xml
    );
}

// --- status topics

/// Motion that was `on` when the camera went to sleep (or dropped) is published `off`;
/// before, it stayed `on` until the next motion
#[tokio::test]
async fn test_motion_off_after_disconnect() {
    let mut h = Harness::start(FakeCameraConfig::default(), "idle_grace_s = 1").await;
    let instance = h.neocam.subscribe().await.unwrap();
    let permit = instance.permit("test:wake").await.unwrap();
    wait_stats(&h.cam, 10, |s| {
        s.messages.contains_key(&MSG_ID_MOTION_REQUEST)
    })
    .await;
    h.cam.motion(true);
    h.wait_published(5, |(t, m, _)| {
        t == "neolink/fake/status/motion" && m == "on"
    })
    .await;
    drop(permit);
    h.cam.drop_connections();
    h.wait_published(10, |(t, m, _)| {
        t == "neolink/fake/status/motion" && m == "off"
    })
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
    assert!(
        json.contains("\"reason\":\"mqtt:control/wakeup\""),
        "{}",
        json
    );
    h.send("control/wakeup", "0");
    h.wait_published(10, |(t, m, _)| {
        t == "neolink/fake/status/wakes" && m.contains("\"awake\":false")
    })
    .await;
}

// --- neolink/config writes: power settings come from the config file (SPEC P14)

const FILE_CONFIG: &str = r#"
[mqtt]
broker_addr = "127.0.0.1"
port = 1883

[[cameras]]
name = "garden"
username = "admin"
password = "camsecret"
address = "192.168.1.10:9000"
battery = true
idle_grace_s = 15

[[cameras]]
name = "porch"
username = "admin"
password = "camsecret"
address = "192.168.1.11:9000"

[[cameras]]
name = "attic"
username = "admin"
password = "camsecret"
address = "192.168.1.12:9000"
battery = false
enabled = false
"#;

fn file_config() -> crate::config::Config {
    toml::from_str(FILE_CONFIG).unwrap()
}

/// What an MQTT client would write: the config neolink publishes on `neolink/config`
/// (no passwords), edited by `edit`
fn edited_write(edit: impl FnOnce(&mut toml::Table)) -> String {
    let published = toml::to_string(&file_config()).unwrap();
    let mut table: toml::Table = published.parse().unwrap();
    edit(&mut table);
    toml::to_string(&table).unwrap()
}

fn camera<'a>(table: &'a mut toml::Table, name: &str) -> &'a mut toml::Table {
    table["cameras"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .map(|c| c.as_table_mut().unwrap())
        .find(|c| c["name"].as_str() == Some(name))
        .unwrap()
}

fn write(text: &str) -> ConfigWrite {
    let file = file_config();
    prepare_config_write(text, &file, &file)
}

/// The write is refused whole, naming each camera and key, without echoing secrets
fn assert_refused(text: &str, cameras_keys: &[(&str, &str)]) {
    match write(text) {
        ConfigWrite::Refused(why) => {
            assert!(why.starts_with("FAIL:"), "{}", why);
            for (cam, key) in cameras_keys {
                assert!(
                    why.contains(cam) && why.contains(key),
                    "{}/{}: {}",
                    cam,
                    key,
                    why
                );
            }
            assert!(!why.contains("secret"), "{}", why);
        }
        other => panic!("not refused: {:?}", other),
    }
}

fn applied(text: &str) -> crate::config::Config {
    match write(text) {
        ConfigWrite::Apply(c) => *c,
        other => panic!("not applied: {:?}", other),
    }
}

#[test]
fn test_config_write_cannot_clear_battery() {
    assert_refused(
        &edited_write(|t| {
            camera(t, "garden").insert("battery".into(), false.into());
        }),
        &[("garden", "battery")],
    );
    // Unset sleeps like battery = true, but it is still not the file's value
    assert_refused(
        &edited_write(|t| {
            camera(t, "garden").remove("battery");
        }),
        &[("garden", "battery")],
    );
    assert_refused(
        &edited_write(|t| {
            camera(t, "porch").insert("battery".into(), false.into());
        }),
        &[("porch", "battery")],
    );
}

#[test]
fn test_config_write_cannot_set_idle_disconnect() {
    for key in ["idle_disconnect", "idle", "idle_disc"] {
        for value in [false, true] {
            assert_refused(
                &edited_write(|t| {
                    camera(t, "garden").insert(key.into(), value.into());
                }),
                &[("garden", "idle_disconnect")],
            );
        }
    }
}

#[test]
fn test_config_write_cannot_change_wake_settings() {
    for (key, value) in [
        ("idle_grace_s", toml::Value::from(600)),
        ("idle_grace_s", toml::Value::from(0)),
        ("motion_hold_max_s", toml::Value::from(86400)),
        ("snapshot_fresh", toml::Value::from(true)),
    ] {
        assert_refused(
            &edited_write(|t| {
                camera(t, "porch").insert(key.into(), value);
            }),
            &[("porch", key)],
        );
    }
    // Removing idle_grace_s = 15 changes it to the default
    assert_refused(
        &edited_write(|t| {
            camera(t, "garden").remove("idle_grace_s");
        }),
        &[("garden", "idle_grace_s")],
    );
}

#[test]
fn test_config_write_cannot_add_pause_on_motion() {
    assert_refused(
        &edited_write(|t| {
            camera(t, "garden")["pause"]
                .as_table_mut()
                .unwrap()
                .insert("on_motion".into(), true.into());
        }),
        &[("garden", "pause.on_motion")],
    );
}

#[test]
fn test_config_write_cannot_enable_a_disabled_camera() {
    assert_refused(
        &edited_write(|t| {
            camera(t, "attic").insert("enabled".into(), true.into());
        }),
        &[("attic", "enabled")],
    );
    // Disabling one is allowed: it only means fewer wakes
    let c = applied(&edited_write(|t| {
        camera(t, "porch").insert("enabled".into(), false.into());
    }));
    assert!(
        !c.cameras
            .iter()
            .find(|c| c.name == "porch")
            .unwrap()
            .enabled
    );
}

#[test]
fn test_config_write_refuses_whole_write() {
    // Several cameras and keys, mixed with an allowed change: nothing is applied
    assert_refused(
        &edited_write(|t| {
            let g = camera(t, "garden");
            g.insert("battery".into(), false.into());
            g.insert("idle_disconnect".into(), false.into());
            let p = camera(t, "porch");
            p["mqtt"]
                .as_table_mut()
                .unwrap()
                .insert("enable_motion".into(), false.into());
            p.insert("snapshot_fresh".into(), true.into());
        }),
        &[
            ("garden", "battery"),
            ("garden", "idle_disconnect"),
            ("porch", "snapshot_fresh"),
        ],
    );
}

#[test]
fn test_config_write_new_camera_takes_default_power() {
    let new_camera = |extra: &[(&str, toml::Value)]| {
        edited_write(|t| {
            let mut cam = toml::Table::new();
            cam.insert("name".into(), "shed".into());
            cam.insert("username".into(), "admin".into());
            cam.insert("password".into(), "newsecret".into());
            cam.insert("address".into(), "192.168.1.13:9000".into());
            for (k, v) in extra {
                cam.insert(k.to_string(), v.clone());
            }
            t["cameras"]
                .as_array_mut()
                .unwrap()
                .push(toml::Value::Table(cam));
        })
    };
    assert_refused(
        &new_camera(&[("battery", false.into())]),
        &[("shed", "battery")],
    );
    assert_refused(
        &new_camera(&[("idle", false.into()), ("idle_grace_s", 3600.into())]),
        &[("shed", "idle_disconnect"), ("shed", "idle_grace_s")],
    );
    // Without power keys it is added, and sleeps
    let c = applied(&new_camera(&[]));
    let shed = c.cameras.iter().find(|c| c.name == "shed").unwrap();
    assert!(shed.sleeps());
}

#[test]
fn test_config_write_cannot_launder_power_through_removal() {
    // garden is in the config file: removed by one write and added back by another, it
    // still has the file's power settings
    let file = file_config();
    let mut running = file.clone();
    running.cameras.retain(|c| c.name != "garden");
    let readd = edited_write(|t| {
        camera(t, "garden").insert("battery".into(), false.into());
    });
    match prepare_config_write(&readd, &file, &running) {
        ConfigWrite::Refused(why) => assert!(why.contains("garden"), "{}", why),
        other => panic!("not refused: {:?}", other),
    }
}

#[test]
fn test_config_write_other_changes_apply() {
    // What Home Assistant does: toggle an MQTT feature
    let c = applied(&edited_write(|t| {
        camera(t, "garden")["mqtt"]
            .as_table_mut()
            .unwrap()
            .insert("enable_motion".into(), false.into());
    }));
    let garden = c.cameras.iter().find(|c| c.name == "garden").unwrap();
    assert!(!garden.mqtt.enable_motion);
    assert_eq!(garden.battery, Some(true));
    assert_eq!(garden.idle_grace_s, 15);
    // The password was restored: same camera address
    assert_eq!(garden.password.as_deref(), Some("camsecret"));

    // The same power values, written explicitly (one through an alias), with another change
    let c = applied(&edited_write(|t| {
        let p = camera(t, "porch");
        p.insert("idle_grace_s".into(), 10.into());
        p.insert("motion_hold_max_s".into(), 120.into());
        p.insert("snapshot_fresh".into(), false.into());
        p.insert("stream".into(), "Main".into());
        let a = camera(t, "attic");
        a.remove("enabled");
        a.insert("enable".into(), false.into());
    }));
    let porch = c.cameras.iter().find(|c| c.name == "porch").unwrap();
    assert_eq!(porch.stream, crate::config::StreamConfig::Main);
}

#[test]
fn test_config_write_same_config_is_unchanged() {
    let text = edited_write(|_| {});
    assert!(matches!(write(&text), ConfigWrite::Unchanged));
}
