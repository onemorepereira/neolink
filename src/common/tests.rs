//! Tests of the camera instance machinery against `neolink_core::test_util::FakeCamera`

use super::*;
use crate::config::CameraConfig;
use neolink_core::test_util::{FakeCamera, FakeCameraConfig, FakeStats};
use std::time::Duration;

/// A camera config that reaches `cam` over TCP, with extra TOML lines
pub(crate) fn config_for(cam: &FakeCamera, extra: &str) -> CameraConfig {
    let text = format!(
        "name = \"fake\"\nusername = \"admin\"\npassword = \"fakepass\"\naddress = \"{}\"\n{}",
        cam.addr(),
        extra
    );
    toml::from_str(&text).unwrap()
}

pub(crate) async fn new_neocam(config: CameraConfig) -> NeoCam {
    #[cfg(feature = "pushnoti")]
    let neocam = {
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        NeoCam::new(config, tx).await.unwrap()
    };
    #[cfg(not(feature = "pushnoti"))]
    let neocam = NeoCam::new(config).await.unwrap();
    neocam
}

/// Wait until `pred` holds for the fake's stats, or panic after `secs` seconds
pub(crate) async fn wait_stats(cam: &FakeCamera, secs: u64, pred: impl Fn(&FakeStats) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let stats = cam.stats();
        if pred(&stats) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out, stats: {:?}",
            stats
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn test_run_task_reaches_camera() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let neocam = new_neocam(config_for(&cam, "")).await;
    let instance = neocam.subscribe().await.unwrap();
    let uid = instance
        .run_task(|cam| Box::pin(async move { Ok(cam.uid().await?) }))
        .await
        .unwrap();
    assert_eq!(uid, "FAKEUID0000000001");
    assert!(cam.stats().logins >= 1);
}

#[tokio::test]
async fn test_reconnects_after_drop() {
    let _ = env_logger::builder().is_test(true).try_init();
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let neocam = new_neocam(config_for(&cam, "idle_disconnect = false")).await;
    let instance = neocam.subscribe().await.unwrap();
    wait_stats(&cam, 10, |s| s.logins >= 1).await;
    cam.drop_connections();
    // The next task must be served by a new login
    let battery = tokio::time::timeout(
        Duration::from_secs(20),
        instance.run_task(|cam| Box::pin(async move { Ok(cam.battery_info().await?) })),
    )
    .await
    .expect("task did not recover from the drop")
    .unwrap();
    assert_eq!(battery.battery_percent, 80);
    assert!(cam.stats().logins >= 2);
}

// ---------------------------------------------------------------------------------------
// Power (SPEC §4.1.1)

/// P1: a sleeping camera is not contacted at boot, even after an hour
#[tokio::test(start_paused = true)]
async fn test_boot_makes_no_connection() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let _neocam = new_neocam(config_for(&cam, "")).await;
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(cam.stats(), FakeStats::default());
}

/// Always-on must be asked for, and then it connects at boot
#[tokio::test]
async fn test_always_on_connects_at_boot() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let _neocam = new_neocam(config_for(&cam, "idle_disconnect = false")).await;
    wait_stats(&cam, 10, |s| s.logins == 1).await;
}

/// A known mains camera stays connected unless idle_disconnect is set
#[tokio::test]
async fn test_mains_camera_stays_connected() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let _neocam = new_neocam(config_for(&cam, "battery = false")).await;
    wait_stats(&cam, 10, |s| s.logins == 1).await;
}

/// P2, P13: a task wakes the camera, which is released with a logout after idle_grace_s
#[tokio::test]
async fn test_task_wakes_then_releases_cleanly() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let neocam = new_neocam(config_for(&cam, "idle_grace_s = 1")).await;
    let instance = neocam.subscribe().await.unwrap();
    let percent = instance
        .run_task(|cam| Box::pin(async move { Ok(cam.battery_info().await?.battery_percent) }))
        .await
        .unwrap();
    assert_eq!(percent, 80);
    assert_eq!(cam.stats().logins, 1);
    wait_stats(&cam, 10, |s| s.open_connections == 0).await;
    let s = cam.stats();
    assert_eq!(s.logouts, 1, "released without a logout: {:?}", s);
    // No probe login: one connection for discovery, one for the session
    assert_eq!(s.failed_logins, 0);
    assert_eq!(s.connections, 2);

    let wakes = neocam.ledger().lock().unwrap().wakes();
    assert_eq!(wakes.len(), 1);
    assert!(
        wakes[0].reason.starts_with("task:src/common/tests.rs:"),
        "{:?}",
        wakes
    );
    assert!(wakes[0].awake.is_some());
}

/// P2: a use within the grace period reuses the connection
#[tokio::test]
async fn test_grace_period_reuses_connection() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let neocam = new_neocam(config_for(&cam, "idle_grace_s = 5")).await;
    let instance = neocam.subscribe().await.unwrap();
    for _ in 0..3 {
        instance
            .run_task(|cam| Box::pin(async move { Ok(cam.uid().await?) }))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(cam.stats().logins, 1);
}

/// P4: an unreachable camera is not retried once nothing needs it
#[tokio::test]
async fn test_no_retry_without_demand() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    cam.set_unreachable(true);
    let neocam = new_neocam(config_for(&cam, "idle_grace_s = 1")).await;
    let instance = neocam.subscribe().await.unwrap();
    let task = tokio::time::timeout(
        Duration::from_secs(4),
        instance.run_task(|cam| Box::pin(async move { Ok(cam.uid().await?) })),
    )
    .await;
    assert!(task.is_err(), "the task should still be waiting");
    // Demand ended when the task was dropped; after the grace period attempts stop
    tokio::time::sleep(Duration::from_secs(3)).await;
    let attempts = cam.stats().connections;
    assert!(attempts >= 1);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(
        cam.stats().connections,
        attempts,
        "kept retrying without demand"
    );
}
